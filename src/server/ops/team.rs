//! Team writes: add an overlay teammate, remove one, set a teammate's daily
//! spend cap, and toggle its inbox — under both scope forms.
//!
//! Adds use the **operator-overlay** model: a new teammate is persisted as an
//! [`OverlayAgent`](crate::ports::types::OverlayAgent) on the `CompanyRecord`
//! through [`CompanyStore`](crate::ports::CompanyStore) and merged into the
//! roster at read time; the version-controlled `company.toml` is never
//! rewritten. A teammate defined in the manifest cannot be removed here (409).
//!
//! ## Daily budgets (issue #343)
//!
//! `budget_usd_daily` is enforced (issue #304) but was readable only from the
//! manifest, which on a hosted tenant is baked into the container image — so an
//! operator whose teammate hit its cap had no remedy short of a redeploy.
//! `PUT`/`DELETE …/team/{agent_id}/budget` write a
//! [`BudgetOverride`](crate::ports::types::BudgetOverride) onto the record, and
//! [`CompanyRecord::effective_budget`](crate::ports::types::CompanyRecord::effective_budget)
//! resolves it ahead of the manifest everywhere the cap is read. The harness
//! fingerprints the override set, so the new value is enforced on the company's
//! next dispatch with no restart.
//!
//! Three rules the surface exists to keep:
//!
//! - **Admin-only, and attributed.** Raising your own spend limit is a privilege
//!   boundary, so both writes go through
//!   [`require_admin`](crate::server::users::admin::require_admin) and stamp who
//!   did it and when.
//! - **Clearing is not zeroing.** `{"budgetUsdDaily": null}` removes the cap;
//!   `{"budgetUsdDaily": 0}` caps at nothing. They are different stored states
//!   and different behaviours. An **omitted** key is neither, so it is a 422
//!   rather than a silent uncap — see [`SetBudget`].
//! - **Reset is its own verb.** `DELETE` drops the override so the manifest
//!   default applies again, which no `PUT` body can express.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::dns::DomainStatus;
use crate::company::setup::AgentFocus;
use crate::error::OpenCompanyError;
use crate::ports::inbox::InboxMeta;
use crate::ports::now_millis;
use crate::ports::store::company_write_lock;
use crate::ports::types::{
    Actor, ActorKind, AgentOverride, BudgetOverride, CompanyRecord, OverlayAgent,
};
use crate::server::error::ApiError;
use crate::server::ops::language;
use crate::server::ops::{DOMAIN_KEY, ScopedCompany, scoped};
use crate::server::users::admin::require_admin;

/// Builds the team route fragment.
pub fn router() -> Router<AppState> {
    scoped("/team", get(list_team).post(add_member))
        // `GET`/`PATCH` come from the sibling `team_agent` module (issue #264)
        // and are attached to this one entry rather than merged in as a second
        // router: axum panics when two routers claim the same path, even for
        // disjoint methods.
        .merge(scoped(
            "/team/{agent_id}",
            super::team_agent::method_router().delete(remove_member),
        ))
        // Issue #1776: the same drafting for a teammate that does not exist
        // yet — the Add-teammate form, which has no id to address. A static
        // segment, so it shadows nothing: no `POST` is served on
        // `/team/{agent_id}`, and a teammate whose id really is `draft` drafts
        // at `/team/draft/draft`.
        .merge(scoped(
            "/team/draft",
            post(super::team_agent::draft_new_profile),
        ))
        // Issue #1776: drafting a mandate or persona for one teammate. Its own
        // path rather than another method on `/team/{agent_id}`, because it is
        // not a write to that teammate — it reads the record and returns text,
        // and a `POST` on the teammate's own path would read as one.
        .merge(scoped(
            "/team/{agent_id}/draft",
            post(super::team_agent::draft_profile),
        ))
        .merge(scoped("/team/{agent_id}/inbox", put(toggle_inbox)))
        .merge(scoped(
            "/team/{agent_id}/budget",
            put(set_budget).delete(clear_budget),
        ))
}

/// One teammate as the console renders it (mirrors `TeamMemberDto`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TeamMemberDto {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// The declared cognition-tier hint (`[[agent]].tier`) **verbatim**, absent
    /// when this teammate declares none — from the same constructor as
    /// `GET …/team/{agent_id}` (issue #643).
    ///
    /// Carried on the list for the reason `tools` and `desks` are: the overview
    /// graph is built from the roster read, so a field the list omitted was a
    /// field the graph had to invent. It invented this one as a literal
    /// `worker` on every node, and a company declaring `tier = "orchestrator"`
    /// read back as a worker on its own graph.
    ///
    /// Absent is a real answer — "this teammate declares no tier" — and is why
    /// the key is skipped rather than defaulted. A default here is precisely
    /// the bug: it is indistinguishable from a declaration on the wire.
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<String>,
    /// Whether this teammate is the company's orchestrator, resolved by the
    /// roster rule (tagged tier first, else the first declared agent) — the
    /// same field, from the same helper, as the detail read (issue #643).
    ///
    /// **Not** derivable from `tier`, which is why it is sent rather than left
    /// to the client: a company that tags nobody still has an orchestrator (no
    /// tier, `true` here), and a second agent tagged with the orchestrator tier
    /// is not one (tier present, `false` here). Always sent, so a client never
    /// has to guess — unlike `tier`, "no orchestrator" is not a state a company
    /// with a roster can be in.
    is_orchestrator: bool,
    /// This teammate's tool grants, in the **same shape and from the same
    /// constructor** as `GET …/team/{agent_id}` (issue #601).
    ///
    /// Carried on the list because the overview knowledge graph draws one ring
    /// per teammate's tools and had no way to learn them: the detail read
    /// answered per agent, so drawing a whole roster meant N+1 fetches on page
    /// load, and the graph invented a tool shelf instead — dealing each
    /// teammate a slice of `[tools].allow` while the detail card beside it
    /// rendered the real grant. One list read now answers for the roster.
    ///
    /// `companyAllow` repeats on every row, which is the payload cost of
    /// mirroring the detail shape exactly rather than inventing a leaner
    /// parallel one. It is worth paying: `requested` is three-state since issue
    /// #1804 (`null` = the company's standard grant, `[]` = an explicit no-tools
    /// grant, `[globs]` = narrow), and a row that dropped the ceiling would leave
    /// a client no way to say which of the three it was looking at.
    tools: super::team_agent::AgentToolsDto,
    /// The desks this teammate sits on, resolved through the same helper the
    /// detail read uses (issue #601). Desks are the company's real grouping —
    /// the overview graph draws its department pillars from these.
    desks: Vec<super::team_agent::AgentDeskDto>,
    /// Whether this teammate has an enabled inbox, so the Team page's toggle
    /// renders the host's real state instead of a client-side guess.
    inbox_enabled: bool,
    /// This teammate's daily spend cap in force (issue #304), or absent when it
    /// has none.
    ///
    /// Absent-vs-present **is** the capped/uncapped distinction, which is why
    /// this is skipped rather than zeroed: `0` would read as "capped at nothing"
    /// and render a permanently exhausted teammate.
    ///
    /// Since #343 this is the **effective** cap — an operator override when one
    /// is stored, the manifest value otherwise — so the card shows what the
    /// dispatch gate will actually enforce. An overlay teammate can carry one
    /// too; it is no longer unconditionally uncapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_usd_daily: Option<f64>,
    /// What this teammate has spent since 00:00 UTC, present only alongside a
    /// cap — an uncapped teammate's spend belongs on the Usage page, not here.
    #[serde(skip_serializing_if = "Option::is_none")]
    spent_today_usd: Option<f64>,
    /// The user id of the admin who last set this teammate's cap from the
    /// console (issue #343), absent when no override is stored.
    ///
    /// Present **whenever an override exists**, including one that removed the
    /// cap — which is why it is not paired with `budgetUsdDaily`. "Nobody has
    /// touched this" and "an admin deliberately uncapped this" look identical
    /// on the cap alone, and the second is exactly what an operator asking
    /// "why is this teammate spending freely?" needs to see.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_set_by: Option<String>,
    /// When that cap was set (epoch millis). Paired with `budgetSetBy`.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_set_at_millis: Option<u64>,
    /// The face this teammate wears, when somebody has chosen one — a
    /// `tiny:<flavour>` mascot or a `blob:<nodeId>` upload
    /// (`docs/spec/runtime/avatars.md`). Absent means **nobody has chosen**, and
    /// the console draws the mascot it hashes from the id.
    ///
    /// Skipped rather than defaulted for the reason `tier` is: absent is a real
    /// answer here, and a client that could not tell it from a choice would have
    /// no way to offer "reset to the default face".
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar: Option<String>,
    /// Whether this teammate came from the **global baseline**
    /// (`docs/spec/runtime/globals.md`) rather than from this company — the
    /// same `Agent::global` marker the merge itself sets (issue #1404).
    ///
    /// Always sent, never skipped. The console's first-run gate asks "has
    /// anybody staffed this company?", and the baseline is appended to every
    /// company whatever its manifest says, so a row that omitted this would be
    /// counted as staff and first-run setup could never open. Absence has to
    /// mean "this host predates the field", which the console reads as the old
    /// behaviour; it must not also mean "not global".
    global: bool,
}

/// The add-teammate body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddMember {
    name: String,
    role: String,
    #[serde(default)]
    description: Option<String>,
    /// An optional daily spend cap for the new teammate (issue #343): "a
    /// teammate created through the Console can be given a cap at creation".
    ///
    /// Only `Some` requires an admin — a budget-less add keeps working for any
    /// member exactly as before, so adding the field takes no permission away.
    #[serde(default)]
    budget_usd_daily: Option<f64>,
    /// An optional per-teammate tool grant (issue #661 / L5): tool-namespace
    /// globs INTERSECTED with the company's `[tools].allow` at roster-build time
    /// — narrow-only, never a widen. Omitted or empty gives the standard
    /// company-wide grant, so adding the field takes no permission away: it can
    /// only restrict the new teammate below what the company already allows.
    #[serde(default)]
    tools: Vec<String>,
    /// An optional face for the new teammate — a `tiny:<flavour>` mascot or a
    /// `blob:<nodeId>` upload (`docs/spec/runtime/avatars.md`), so a teammate can
    /// be born wearing the face the operator picked in the create dialog rather
    /// than flashing a hashed one until a second PATCH lands.
    ///
    /// A plain `Option` for the same reason `instructions` is one: at creation
    /// there is nothing to reset to, so `null` and omitted are the same thing —
    /// the hashed default.
    #[serde(default)]
    avatar: Option<String>,
    /// The job shape that decides this teammate's tool belt, sent by the
    /// first-run setup build-out (issue #1674). When present it derives the
    /// grant list through
    /// [`tools_for_focus`](crate::company::setup::tools_for_focus) — the same
    /// host-side belt table the roster proposal uses — instead of `tools`, so a
    /// setup-created teammate gets the belt its shape was approved with on the
    /// review screen rather than inheriting the whole company default. An
    /// unreadable value fails closed to the Writing belt, exactly as the
    /// proposal's [`focus_from_wire`](crate::company::setup) does; the derived
    /// list is still intersected with the company `[tools].allow` like any
    /// other `tools` line, so this can only ever narrow. Takes no permission:
    /// the setup flow that sends it is the same member-level add as before.
    #[serde(default)]
    focus: Option<String>,
    /// Optional persona instructions for the new teammate (issue #1530), so a
    /// teammate can be born with an overridden persona rather than needing a
    /// second PATCH. A plain `Option` — at creation there is no blueprint to
    /// reset to, so `null`/omitted both mean "no override" and a blank string is
    /// dropped. Takes no permission: it can only add persona text to a teammate
    /// this same call is creating.
    #[serde(default)]
    instructions: Option<String>,
}

/// The set-budget body.
///
/// `budget_usd_daily` is a **double option** so the three cases stay apart on
/// the wire, which is the whole point of the route:
///
/// | body | parses as | means |
/// |---|---|---|
/// | `{"budgetUsdDaily": 5}` | `Some(Some(5.0))` | cap at $5/day |
/// | `{"budgetUsdDaily": 0}` | `Some(Some(0.0))` | cap at nothing |
/// | `{"budgetUsdDaily": null}` | `Some(None)` | remove the cap |
/// | `{}` | *rejected* | — |
///
/// The last row is deliberate. There is **no `#[serde(default)]`**, so an
/// omitted key is a deserialization failure and axum answers `422` — an empty
/// body can never be read as "uncap this teammate". A client that means to
/// remove a cap has to say `null` and mean it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SetBudget {
    #[serde(deserialize_with = "double_option")]
    budget_usd_daily: Option<Option<f64>>,
}

/// Deserializes into `Some(inner)` when the field is present (so an explicit
/// `null` becomes `Some(None)`). Without a companion `#[serde(default)]` an
/// omitted field stays an error — which is what [`SetBudget`] wants.
pub(crate) fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer).map(Some)
}

/// The inbox-toggle body.
#[derive(Debug, Deserialize)]
struct ToggleInbox {
    enabled: bool,
}

/// The inbox-toggle response.
#[derive(Debug, Serialize)]
struct InboxAck {
    key: String,
    address: String,
}

/// The sub-resource path (`agent_id`). Shared with the sibling `team_agent`
/// module, which serves `GET`/`PATCH` on the same path (issue #264).
#[derive(Debug, Deserialize)]
pub(super) struct AgentPath {
    pub(super) agent_id: String,
}

/// `GET {scope}/team` — the merged roster: manifest teammates (versioned in
/// `company.toml`, `name: null` — the console falls back to the role) plus
/// operator-added overlay teammates (`name` always set). Mirrors the GraphQL
/// `resolve_team` merge, `inbox_enabled` included — the console's Team page is
/// its REST consumer, so the inbox toggle reflects the [`InboxStore`] rather
/// than a client-side guess (issue #173). Hosts with no persisted record yet
/// return an empty roster, the same soft-fail the sibling `/desks` route uses,
/// rather than 404ing.
///
/// [`InboxStore`]: crate::ports::InboxStore
async fn list_team(company: ScopedCompany) -> Result<Json<Vec<TeamMemberDto>>, ApiError> {
    let record = company.runtime.store().load(company.id()).await?;
    // Inbox metadata is keyed by agent id, so the roster can be tagged without
    // a per-teammate read. An inbox that was never toggled is simply absent.
    let enabled_inboxes: std::collections::HashMap<String, bool> = company
        .runtime
        .inbox()
        .inboxes(company.id())
        .await?
        .into_iter()
        .map(|meta| (meta.key, meta.enabled))
        .collect();
    let enabled = |id: &str| enabled_inboxes.get(id).copied().unwrap_or(false);
    // Issue #304: today's spend, for capped teammates only. One meter read for
    // the whole roster, and only when the manifest actually caps somebody —
    // a company with no caps pays nothing for a column it will not render.
    let spend_today = daily_spend_samples(&company, record.as_ref()).await?;
    let spent = |id: &str| {
        spend_today
            .as_ref()
            .map(|samples| crate::metering::usd_spent_by_agent(samples, id))
    };
    let members = record
        .map(|record| {
            // Resolved through the record, so a manifest teammate an operator
            // has edited from the console lists under the name, role and
            // description it now has rather than the ones `company.toml`
            // launched it with.
            let mut members: Vec<TeamMemberDto> = record
                .effective_agents()
                .into_iter()
                .map(|agent| {
                    member_row(
                        &record,
                        &agent.id,
                        agent.name.clone(),
                        agent.role.clone(),
                        agent.description.clone(),
                        enabled(&agent.id),
                        &spent,
                    )
                })
                .collect();
            members.extend(record.overlay_agents.iter().map(|agent| {
                member_row(
                    &record,
                    &agent.id,
                    Some(agent.name.clone()),
                    agent.role.clone(),
                    agent.description.clone(),
                    enabled(&agent.id),
                    &spent,
                )
            }));
            members
        })
        .unwrap_or_default();
    Ok(Json(members))
}

/// Builds one roster row, resolving the cap and its attribution through the
/// record so the manifest arm and the overlay arm cannot drift (issue #343).
///
/// `spent` is the shared per-agent spend lookup — `None` for a company where
/// nobody is capped, in which case the meter was never read.
fn member_row(
    record: &CompanyRecord,
    agent_id: &str,
    name: Option<String>,
    role: String,
    description: Option<String>,
    inbox_enabled: bool,
    spent: &dyn Fn(&str) -> Option<f64>,
) -> TeamMemberDto {
    let cap = record.effective_budget(agent_id);
    let attribution = record.budget_override(agent_id);
    TeamMemberDto {
        id: agent_id.to_string(),
        name,
        role,
        description,
        // All four through `team_agent`'s helpers, never recomputed here: the
        // roster list and the detail read must not be able to disagree about
        // the same teammate (issues #264, #601, #643). A second copy of the
        // orchestrator rule in particular would be a copy of a rule that has
        // two arms, and the arm it dropped would be invisible on screen.
        tier: super::team_agent::declared_tier(record, agent_id),
        is_orchestrator: super::team_agent::is_orchestrator(record, agent_id),
        tools: super::team_agent::agent_tools(record, agent_id),
        desks: super::team_agent::desks_for(record, agent_id),
        inbox_enabled,
        budget_usd_daily: cap,
        // Paired with the cap: no cap, no spend row.
        spent_today_usd: cap.and_then(|_| spent(agent_id)),
        budget_set_by: attribution.map(|entry| entry.set_by.id.clone()),
        budget_set_at_millis: attribution.map(|entry| entry.at_millis),
        // Resolved through the record, like every other overlay-backed field:
        // one override row answers for a manifest teammate and an overlay one
        // alike, so both arms of the list above get the chosen face with no
        // second lookup to keep in step.
        avatar: record.effective_avatar(agent_id),
        // Through the same helper as the four above, for the same reason: the
        // roster read is what the first-run gate is decided on, so a second
        // copy of the provenance rule here is a second thing to forget.
        global: super::team_agent::is_global(record, agent_id),
    }
}

/// Today's usage samples (since 00:00 UTC), or `None` when no teammate on this
/// company carries a daily cap (issue #304).
///
/// Returning `None` rather than an empty vec keeps "nobody is capped" distinct
/// from "everybody is capped and has spent nothing", and is what lets the
/// caller skip the meter round-trip entirely for the common uncapped company.
///
/// The scan runs over **effective** caps across the **whole** roster (issue
/// #343). Both halves matter: an override that caps a previously-uncapped
/// teammate has to start the meter read, or its card would render a cap with no
/// spend beside it; and overlay teammates are now cappable, so restricting the
/// scan to manifest agents would miss the only capped teammate on a company
/// whose roster was built entirely from the console.
pub(super) async fn daily_spend_samples(
    company: &ScopedCompany,
    record: Option<&CompanyRecord>,
) -> Result<Option<Vec<crate::ports::usage::UsageSample>>, ApiError> {
    let any_capped = record
        .is_some_and(|record| roster_ids(record).any(|id| record.effective_budget(id).is_some()));
    if !any_capped {
        return Ok(None);
    }
    let since = crate::metering::utc_day_start_millis(crate::ports::now_millis());
    let samples = company
        .runtime
        .usage()
        .query(company.id(), since)
        .await
        .map_err(ApiError)?;
    Ok(Some(samples))
}

/// Every roster teammate's id — manifest agents first, then overlay teammates,
/// minus the ones the operator has removed. The same union
/// `CompanyRecord::is_roster_agent` accepts.
fn roster_ids(record: &CompanyRecord) -> impl Iterator<Item = &String> {
    record
        .manifest
        .agents
        .iter()
        .map(|agent| &agent.id)
        .chain(record.overlay_agents.iter().map(|agent| &agent.id))
        .filter(|id| !record.is_retired(id))
}

/// `POST {scope}/team` — add an operator-defined teammate, optionally with a
/// daily spend cap (issue #343).
///
/// The cap and the teammate land in **one** record save, so a company can never
/// end up with a teammate whose intended cap silently failed to apply.
async fn add_member(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(body): Json<AddMember>,
) -> Result<Json<TeamMemberDto>, crate::server::Rejection> {
    // Setting a cap is admin-only, so an add that carries one is too — but an
    // add that does not keeps working for any member, exactly as before. The
    // check is deliberately conditional: adding this field must not quietly
    // take the existing capability away from members.
    let mut author = match body.budget_usd_daily {
        Some(cap) => {
            if let Some(refusal) = validate_cap(cap) {
                return Err(refusal.into());
            }
            Some(require_admin(&headers, &state, &company.runtime, peer).await?)
        }
        None => None,
    };

    // A create-time face, resolved *before* the write lock below is taken.
    //
    // A `blob:` avatar streams up to 4 MiB from the workspace backend, and the
    // bytes it resolves to do not depend on the record — so holding the
    // per-company write lock across that I/O would let a slow or stalled remote
    // store block every other roster and policy write, on a request any member
    // can repeat. The immutable reference is resolved here instead, and the
    // lock below is held only for the load-mutate-save of the record. (Same
    // shape as `edit_agent` in `team_agent.rs`.)
    //
    // Blank is dropped rather than stored — "no choice" is the hashed default.
    let resolved_avatar: Option<String> = match body
        .avatar
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => {
            let stored = crate::company::avatar::resolve(
                company.runtime.workspace().as_ref(),
                company.id(),
                value,
            )
            .await
            .map_err(|e| ApiError(e).into_response())?;
            Some(stored)
        }
        None => None,
    };

    // Serialize per-company writes so concurrent console POST /team and
    // orchestrator add_agent calls can't clobber each other's overlay_agents.
    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    // Issue #661 / L5: trim + drop blank globs, mirroring the orchestrator
    // `add_agent` parse. Empty stays empty → the standard company-wide grant.
    //
    // Issue #1674: a `focus` from the setup build-out derives the grant list
    // host-side instead — the belt table lives in `src/company/setup.rs`, and
    // the console has no business choosing a permission boundary. An unreadable
    // focus fails closed to the Writing belt (`tools_for_focus`), never wider.
    let mut tools: Vec<String> = match body.focus.as_deref().map(str::trim) {
        Some(focus) if !focus.is_empty() => {
            crate::company::setup::tools_for_focus(AgentFocus::from_wire(focus))
        }
        _ => body
            .tools
            .into_iter()
            .map(|glob| glob.trim().to_string())
            .filter(|glob| !glob.is_empty())
            .collect(),
    };
    // Naming a BYO real-money namespace for a NEW teammate is a billing
    // decision. A budget that spends money is already admin-only above; an
    // explicit `chargebee`/`paypal`/`hosting` grant without a cap must be too —
    // otherwise the day a company's ceiling includes `chargebee`, any member
    // could mint a billing-capable teammate, while editing an existing
    // teammate's `tools` is already admin-only (`team_agent.rs`). Focus-derived
    // belts never name these namespaces, so only a hand-typed grant trips this.
    if author.is_none()
        && tools.iter().any(|grant| {
            let one = std::slice::from_ref(grant);
            crate::company::grants_chargebee_explicit(one)
                || crate::company::grants_paypal_explicit(one)
                || crate::company::grants_hosting_explicit(one)
        })
    {
        author = Some(require_admin(&headers, &state, &company.runtime, peer).await?);
    }
    let mut record = load_record(&company).await?;
    // A teammate created with no stated grant does not inherit the BYO
    // real-money namespaces (#788/#789), even though "empty" otherwise means
    // the standard company-wide grant. A company holds `chargebee` because
    // somebody named it so that ONE teammate could invoice; the next teammate
    // an operator types into the console is not that teammate, and silence is
    // not consent to bill a customer. `creation_default_grants` returns empty
    // — leaving the inherit-everything contract untouched — for every company
    // that grants none of them, which is all but a handful.
    //
    // Deliberately here rather than in the roster build: this materialises the
    // narrowed list ONCE, at creation, so the stored teammate carries its own
    // line. Narrowing at read time instead would silently re-widen the day an
    // operator edited the teammate for an unrelated reason.
    if tools.is_empty() {
        match crate::company::creation_default_grants(&record.manifest.tools.allow) {
            crate::company::CreationGrant::Standard => {}
            crate::company::CreationGrant::Narrowed(narrowed) => tools = narrowed,
            // Nothing safe to store: see `CreationGrant::NothingLeft`. Refusing
            // is the honest answer and the operator can still create the
            // teammate by naming its tools.
            crate::company::CreationGrant::NothingLeft => {
                return Err(ApiError(crate::error::OpenCompanyError::InvalidRequest(
                    "this company grants only billing namespaces, so a teammate created with no \
                     `tools` would inherit them. State the teammate's tools explicitly."
                        .to_string(),
                ))
                .into_response()
                .into());
            }
        }
    }
    let agent = OverlayAgent {
        // A readable id derived from the name, unique against the roster this
        // record already holds (issue #686). Minted here rather than pushed and
        // renamed later: the id names the teammate's `agents/<id>/` folder and
        // stamps every artifact it authors, so it has to be right on the first
        // save. The surrounding write lock is what makes the uniqueness check
        // and the save below one atomic step.
        id: record.mint_agent_id(&body.name),
        name: body.name,
        role: body.role,
        description: body.description,
        // Issue #661 / L5: the teammate's own grant, intersected with the
        // company allow-list by the shared reads/roster build. A teammate created
        // with no stated (and no billing-narrowed) grant is stored as `None` —
        // inherit the company's standard grant — not `Some(vec![])`, which since
        // issue #1804 is an explicit deny-all. This create path expresses only
        // "inherit" and "narrow"; the deny-all state is reachable by editing the
        // teammate afterwards (`PATCH …/team/{id}` with `tools: []`).
        tools: if tools.is_empty() { None } else { Some(tools) },
        model: None,
        harness: None,
    };
    record.overlay_agents.push(agent.clone());
    let attribution = author.map(|admin| BudgetOverride {
        agent_id: agent.id.clone(),
        budget_usd_daily: body.budget_usd_daily,
        set_by: Actor {
            kind: ActorKind::User,
            id: admin.user_id,
        },
        at_millis: now_millis(),
    });
    if let Some(entry) = attribution.clone() {
        // Through the upsert even though `agent.id` is freshly generated and so
        // cannot already hold a row: the "one override per teammate" invariant
        // belongs to the record, not to each call site's reasoning about id
        // uniqueness.
        record.upsert_budget_override(entry);
    }
    // Issue #1530: a create-time persona override, so a teammate can be born with
    // an overridden persona. Trimmed-empty is dropped — a blank string is "no
    // override", never a stored empty persona. Through the upsert for the same
    // invariant-belongs-to-the-record reason as the budget above.
    if let Some(instructions) = body
        .instructions
        .as_deref()
        .map(str::trim)
        .map(crate::company::prompt::cap_persona_instructions)
        .filter(|text| !text.is_empty())
    {
        record.upsert_agent_override(AgentOverride {
            agent_id: agent.id.clone(),
            instructions: Some(instructions),
            ..Default::default()
        });
    }
    // The resolved face is applied to the record under the lock so the upsert
    // lands in the same atomic save as the teammate itself.
    if let Some(stored) = resolved_avatar.clone() {
        record.upsert_agent_override(AgentOverride {
            agent_id: agent.id.clone(),
            avatar: Some(stored),
            ..Default::default()
        });
    }
    company.runtime.store().save(&record).await?;
    // A brand-new overlay teammate has no `[[agent]]` row at all, so it declares
    // no tier, holds the company's standard grant, and sits on no desk until
    // somebody adds it to one. Resolved through the shared helpers rather than
    // written out here, so this response cannot drift from the two reads
    // (issues #601, #643).
    let tier = super::team_agent::declared_tier(&record, &agent.id);
    let is_orchestrator = super::team_agent::is_orchestrator(&record, &agent.id);
    let tools = super::team_agent::agent_tools(&record, &agent.id);
    let desks = super::team_agent::desks_for(&record, &agent.id);
    Ok(Json(TeamMemberDto {
        id: agent.id,
        name: Some(agent.name),
        role: agent.role,
        description: agent.description,
        tier,
        is_orchestrator,
        tools,
        desks,
        // A brand-new teammate has no inbox until the toggle writes one.
        inbox_enabled: false,
        budget_usd_daily: body.budget_usd_daily,
        // Brand new, so nothing has been spent against the cap yet. Sent as
        // `0.0` rather than omitted so the card renders "$0.00 spent today"
        // beside a cap it was just given, instead of a cap with nothing next
        // to it.
        spent_today_usd: body.budget_usd_daily.map(|_| 0.0),
        budget_set_by: attribution.as_ref().map(|entry| entry.set_by.id.clone()),
        budget_set_at_millis: attribution.as_ref().map(|entry| entry.at_millis),
        avatar: resolved_avatar,
        // An operator just created this one, so it is by construction not from
        // the baseline — the merge only ever appends to the manifest roster.
        // It is also exactly the write that closes the first-run gate.
        global: false,
    }))
}

async fn remove_member(
    company: ScopedCompany,
    Path(AgentPath { agent_id }): Path<AgentPath>,
) -> Result<StatusCode, ApiError> {
    // Serialize so a concurrent add_agent / add_member doesn't clobber.
    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;
    if !record.is_roster_agent(&agent_id) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "teammate {agent_id}"
        ))));
    }
    // The one refusal left: a company with nobody on it has no orchestrator, no
    // one to answer a message and no way back from the console. Counted over the
    // roster as it effectively stands, so the check sees the teammates that are
    // actually there rather than the ones the blueprint declared.
    if roster_ids(&record).count() <= 1 {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::LAST_TEAMMATE_DELETE.to_string(),
        )));
    }

    // Tombstone the operator-feed divert before it can be lost (issue #1781
    // review, Codex P2 follow-up to the desk-deletion fix): a manifest
    // teammate at the literal id `operator` is already covered below —
    // `retire_agent` tombstones it under the same key
    // `operator_feed_channel`'s own `is_retired` check reads — but an
    // *overlay* teammate is deleted outright with no tombstone at all. If
    // this removal is what's currently holding the divert (id or, via
    // `is_roster_agent`, nothing else does for a teammate — desks are the
    // only case matched by display name), the fallback address must stay
    // fixed after the removal exactly as `delete_desk` already keeps it
    // fixed after a colliding desk's removal — see
    // `CompanyRecord::divert_operator_feed_permanently`'s doc.
    if record.operator_feed_channel()
        == crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK
    {
        record.divert_operator_feed_permanently();
    }
    let is_manifest = record.manifest.agents.iter().any(|a| a.id == agent_id);
    if is_manifest {
        // A tombstone, not a manifest rewrite: `company.toml` and the global
        // baseline merged into it are re-read on every rebuild, so a teammate
        // "removed" by editing the roster would simply come back. Recorded here
        // and filtered out by `CompanyRecord::effective_agents`, which is what
        // takes the teammate off the roster, off its desks and out of the
        // harness build rather than merely off the Team page.
        record.retire_agent(&agent_id);
    } else {
        record.overlay_agents.retain(|a| a.id != agent_id);
    }
    // Desk seats an operator added are dropped with the teammate either way. A
    // blueprint seat is left alone — `effective_desk_members` already filters a
    // retired teammate out of it, and the manifest is not rewritten.
    record
        .overlay_desk_members
        .retain(|member| member.agent_id != agent_id);
    // The teammate's edit overlay goes with it too, for the same
    // id-reuse reason the budget override below does: the id is a slug of the
    // display name, so a later teammate can take this seat and would otherwise
    // inherit a rename nobody made for it. A retired manifest teammate loses its
    // edits as well — if it ever comes back it comes back as the blueprint
    // declares it.
    record
        .overlay_agent_edits
        .retain(|edit| edit.agent_id != agent_id);
    // Drop the teammate's budget override with it (issue #343). Since #686 the
    // id is a slug of the display name rather than a generated one, so removing
    // a teammate *frees its id*: re-adding the same name mints the same slug and
    // the new teammate adopts the old one's `Agents/<slug>/` folder. Clearing
    // the override here is therefore load-bearing, not just hygiene — a row left
    // behind would silently cap whoever next takes the seat. See
    // `CompanyRecord::mint_agent_id` for why the reuse is the intended remedy
    // for a typo'd name rather than a hazard to design around.
    record.overlay_budgets.retain(|b| b.agent_id != agent_id);
    company.runtime.store().save(&record).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT {scope}/team/{agent_id}/budget` — set, change, or remove a teammate's
/// daily spend cap. Admin-only, attributed, and in force on the next dispatch.
///
/// See [`SetBudget`] for why `{}` is a `422` rather than an uncap.
async fn set_budget(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Path(AgentPath { agent_id }): Path<AgentPath>,
    Json(body): Json<SetBudget>,
) -> Result<Json<TeamMemberDto>, crate::server::Rejection> {
    let admin = require_admin(&headers, &state, &company.runtime, peer).await?;
    // `Some(_)` is guaranteed by `SetBudget`'s missing-key rejection; the inner
    // option is the cap-or-uncap the operator asked for.
    let cap = body.budget_usd_daily.flatten();
    if let Some(refusal) = cap.and_then(validate_cap) {
        return Err(refusal.into());
    }

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;
    if let Some(refusal) = require_roster_teammate(&record, &agent_id) {
        return Err(refusal.into());
    }

    let entry = BudgetOverride {
        agent_id: agent_id.clone(),
        budget_usd_daily: cap,
        set_by: Actor {
            kind: ActorKind::User,
            id: admin.user_id,
        },
        at_millis: now_millis(),
    };
    // One override per teammate: replace in place rather than accumulating, so
    // `effective_budget`'s first-match read can never see a stale row.
    record.upsert_budget_override(entry);
    company.runtime.store().save(&record).await?;

    updated_row(&company, &record, &agent_id).await
}

/// `DELETE {scope}/team/{agent_id}/budget` — drop the override so the manifest
/// default applies again.
///
/// Distinct from `PUT null`, and not expressible by it: `PUT null` stores "no
/// cap, decided by an admin", while this restores whatever `company.toml`
/// declares — which for a manifest-capped teammate means the cap comes **back**.
/// Deleting when nothing is stored is a no-op rather than a 404: the caller's
/// intent ("this teammate should follow the manifest") is already satisfied.
async fn clear_budget(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Path(AgentPath { agent_id }): Path<AgentPath>,
) -> Result<Json<TeamMemberDto>, crate::server::Rejection> {
    require_admin(&headers, &state, &company.runtime, peer).await?;

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;
    if let Some(refusal) = require_roster_teammate(&record, &agent_id) {
        return Err(refusal.into());
    }

    record.overlay_budgets.retain(|b| b.agent_id != agent_id);
    company.runtime.store().save(&record).await?;

    updated_row(&company, &record, &agent_id).await
}

/// Rejects a cap that is not a spendable amount of money, mirroring the
/// manifest validation in `crate::company::manifest` so a value the console
/// accepts is one `company.toml` would have accepted too.
///
/// NaN and the infinities are refused as well as negatives. They parse as JSON
/// numbers in some encoders and would poison every comparison downstream: the
/// dispatch gate's `spent >= cap` is false for NaN, so a NaN cap would read as
/// "capped" everywhere in the console while enforcing nothing at all.
fn validate_cap(cap: f64) -> Option<Response> {
    if !cap.is_finite() {
        return Some(
            ApiError(OpenCompanyError::InvalidRequest(
                "a daily budget has to be a real number of dollars.".to_string(),
            ))
            .into_response(),
        );
    }
    if cap < 0.0 {
        return Some(
            ApiError(OpenCompanyError::InvalidRequest(format!(
                "a daily budget cannot be negative — you sent `{cap}`."
            )))
            .into_response(),
        );
    }
    None
}

/// Loads the addressed company's record, or 404s.
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

/// 404s unless `agent_id` names a real roster teammate.
///
/// Without this an unknown id would store an override nothing ever reads —
/// a write that reports success and changes nothing, which is worse than a
/// refusal because the operator believes the cap is in place.
fn require_roster_teammate(record: &CompanyRecord, agent_id: &str) -> Option<Response> {
    if record.is_roster_agent(agent_id) {
        return None;
    }
    Some(
        ApiError(OpenCompanyError::CompanyNotFound(format!(
            "teammate {agent_id}"
        )))
        .into_response(),
    )
}

/// The teammate's roster row after a budget write, so the console can update the
/// card from the response instead of refetching the whole team.
async fn updated_row(
    company: &ScopedCompany,
    record: &CompanyRecord,
    agent_id: &str,
) -> Result<Json<TeamMemberDto>, crate::server::Rejection> {
    let spend_today = daily_spend_samples(company, Some(record))
        .await
        .map_err(|e| e.into_response())?;
    let spent = |id: &str| {
        spend_today
            .as_ref()
            .map(|samples| crate::metering::usd_spent_by_agent(samples, id))
    };
    let inbox_enabled = company
        .runtime
        .inbox()
        .inboxes(company.id())
        .await?
        .into_iter()
        .any(|meta| meta.key == agent_id && meta.enabled);

    // Same rule as `list_team`, and resolved the same way: through the record,
    // so a manifest teammate an operator has edited answers a budget write with
    // the name, role and description it now has. Reading the raw manifest row
    // here would make one card change identity depending on which route last
    // touched it — a rename would show on the roster and vanish the moment a cap
    // was set.
    let overlay = record.overlay_agents.iter().find(|a| a.id == agent_id);
    let (name, role, description) = match overlay {
        Some(agent) => (
            Some(agent.name.clone()),
            agent.role.clone(),
            agent.description.clone(),
        ),
        None => {
            let agent = record
                .effective_agent(agent_id)
                .expect("roster membership was checked before the write");
            (
                agent.name.clone(),
                agent.role.clone(),
                agent.description.clone(),
            )
        }
    };
    Ok(Json(member_row(
        record,
        agent_id,
        name,
        role,
        description,
        inbox_enabled,
        &spent,
    )))
}

async fn toggle_inbox(
    company: ScopedCompany,
    Path(AgentPath { agent_id }): Path<AgentPath>,
    Json(body): Json<ToggleInbox>,
) -> Result<Json<InboxAck>, ApiError> {
    // Resolve a display name and address for the inbox metadata.
    let record = company.runtime.store().load(company.id()).await?;
    let name = record
        .as_ref()
        .and_then(|r| {
            r.manifest
                .agents
                .iter()
                .find(|a| a.id == agent_id)
                .map(|a| a.role.clone())
                .or_else(|| {
                    r.overlay_agents
                        .iter()
                        .find(|a| a.id == agent_id)
                        .map(|a| a.name.clone())
                })
        })
        .unwrap_or_else(|| agent_id.clone());
    let address = match load_domain(&company).await? {
        Some(domain) => format!("{agent_id}@{domain}"),
        None => agent_id.clone(),
    };
    let meta = InboxMeta {
        key: agent_id.clone(),
        name,
        address: address.clone(),
        enabled: body.enabled,
    };
    company
        .runtime
        .inbox()
        .set_enabled(company.id(), &agent_id, &meta)
        .await?;
    Ok(Json(InboxAck {
        key: agent_id,
        address,
    }))
}

/// Loads the configured custom domain, if any.
async fn load_domain(company: &ScopedCompany) -> Result<Option<String>, ApiError> {
    let Some(value) = company
        .runtime
        .secrets()
        .get(company.id(), DOMAIN_KEY)
        .await?
    else {
        return Ok(None);
    };
    let status: DomainStatus = serde_json::from_str(value.expose())?;
    Ok(Some(status.domain))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::CompanyStore;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::ports::usage::{SampleKind, UsageSample};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-team-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
        state_with(home, toml::from_str(manifest_toml).unwrap()).await
    }

    /// As above, but with the **global baseline merged in** — the roster every
    /// company actually boots with (`docs/spec/runtime/globals.md`).
    ///
    /// Kept apart from `state_with_manifest` on purpose: most tests here are
    /// about one hand-written teammate and are clearer without four extra rows,
    /// while the provenance tests are meaningless without them.
    async fn state_with_globals(home: &std::path::Path, manifest_toml: &str) -> AppState {
        let mut manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        manifest.apply_globals();
        state_with(home, manifest).await
    }

    async fn state_with(home: &std::path::Path, manifest: CompanyManifest) -> AppState {
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

    async fn get_team(state: &AppState) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri("/api/v1/company/team")
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

    /// Two teammates on one manifest: `analyst` is capped, `writer` is not.
    const ROSTER: &str = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\nbudget_usd_daily = 5.0\n\
         [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n";

    /// Drives any team route with an explicit cookie, so the auth boundary can
    /// be exercised with an admin session, a member session, or none at all.
    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
        cookie: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let request = match &body {
            Some(value) => builder
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(value).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
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

    fn admin_cookie() -> String {
        crate::server::test_support::fixed_cookie("acme")
    }

    /// `PUT …/team/{id}/budget` as the seeded admin.
    async fn put_budget(state: &AppState, agent: &str, body: Value) -> (StatusCode, Value) {
        send(
            state,
            "PUT",
            &format!("/api/v1/company/team/{agent}/budget"),
            Some(body),
            Some(&admin_cookie()),
        )
        .await
    }

    /// One roster row from `GET …/team`.
    async fn team_row(state: &AppState, agent: &str) -> Value {
        let (status, body) = get_team(state).await;
        assert_eq!(status, StatusCode::OK);
        body.as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == agent)
            .unwrap_or_else(|| panic!("no {agent} row in {body}"))
            .clone()
    }

    // --- Console budget writes (issue #343) ---------------------------------

    /// The acceptance criterion, on the wire: an admin sets, changes and clears
    /// a cap, and every state is visible on the next read.
    ///
    /// The `writer` starts **uncapped in the manifest**, which is the case the
    /// pre-#343 code could not express at all — there was no field to write.
    #[tokio::test]
    async fn an_admin_can_set_change_and_clear_a_cap() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        // Uncapped to begin with — no cap key, no attribution.
        let before = team_row(&state, "writer").await;
        assert!(before.get("budgetUsdDaily").is_none(), "{before}");
        assert!(before.get("budgetSetBy").is_none(), "{before}");

        // Set.
        let (status, row) = put_budget(&state, "writer", json!({"budgetUsdDaily": 12.5})).await;
        assert_eq!(status, StatusCode::OK, "{row}");
        assert_eq!(row["budgetUsdDaily"], 12.5, "{row}");
        let after_set = team_row(&state, "writer").await;
        assert_eq!(after_set["budgetUsdDaily"], 12.5, "{after_set}");
        assert!(
            after_set["budgetSetBy"].is_string(),
            "a set cap is attributable to the admin who set it: {after_set}"
        );
        assert!(
            after_set["budgetSetAtMillis"].as_u64().unwrap() > 0,
            "{after_set}"
        );

        // Change.
        let (status, _) = put_budget(&state, "writer", json!({"budgetUsdDaily": 3.0})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(team_row(&state, "writer").await["budgetUsdDaily"], 3.0);

        // Remove the cap (explicit null).
        let (status, row) = put_budget(&state, "writer", json!({"budgetUsdDaily": null})).await;
        assert_eq!(status, StatusCode::OK, "{row}");
        let uncapped = team_row(&state, "writer").await;
        assert!(
            uncapped.get("budgetUsdDaily").is_none(),
            "an uncapped teammate omits the cap key entirely: {uncapped}"
        );
        assert!(
            uncapped["budgetSetBy"].is_string(),
            "…but the attribution stays, so an operator can see that a human \
             uncapped this teammate rather than that nobody ever capped it: {uncapped}"
        );
    }

    /// A cap set from the console **wins over the manifest**, and `DELETE`
    /// puts the manifest back. This is the pair that makes the override a
    /// remedy rather than a second opinion.
    #[tokio::test]
    async fn an_override_beats_the_manifest_and_delete_restores_it() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        assert_eq!(team_row(&state, "analyst").await["budgetUsdDaily"], 5.0);

        let (status, _) = put_budget(&state, "analyst", json!({"budgetUsdDaily": 50.0})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            team_row(&state, "analyst").await["budgetUsdDaily"],
            50.0,
            "the stored cap wins over the manifest's $5"
        );

        let (status, row) = send(
            &state,
            "DELETE",
            "/api/v1/company/team/analyst/budget",
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{row}");
        let reset = team_row(&state, "analyst").await;
        assert_eq!(
            reset["budgetUsdDaily"], 5.0,
            "DELETE drops the override, so the manifest default applies again: {reset}"
        );
        assert!(
            reset.get("budgetSetBy").is_none(),
            "with no override there is nothing to attribute: {reset}"
        );
    }

    /// The issue's third rule, pinned **on the wire** rather than in Rust: `0`
    /// and `null` are different bodies with different stored outcomes.
    ///
    /// `0` caps the teammate at nothing (the cap key comes back as `0.0`);
    /// `null` removes the cap (the key is absent). If these ever collapsed, an
    /// operator lifting a cap would instead have silenced the teammate.
    #[tokio::test]
    async fn zero_and_null_are_different_states() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, _) = put_budget(&state, "analyst", json!({"budgetUsdDaily": 0})).await;
        assert_eq!(status, StatusCode::OK);
        let zeroed = team_row(&state, "analyst").await;
        assert_eq!(
            zeroed["budgetUsdDaily"], 0.0,
            "a zero cap is sent as 0, not omitted: {zeroed}"
        );

        let (status, _) = put_budget(&state, "analyst", json!({"budgetUsdDaily": null})).await;
        assert_eq!(status, StatusCode::OK);
        let cleared = team_row(&state, "analyst").await;
        assert!(
            cleared.get("budgetUsdDaily").is_none(),
            "a cleared cap omits the key — and beats the manifest's $5: {cleared}"
        );
    }

    /// An omitted key is **not** an uncap. `{}` cannot be mistaken for
    /// `{"budgetUsdDaily": null}`, so a client bug or a truncated body can never
    /// silently lift a cap; axum rejects the body before the handler runs.
    #[tokio::test]
    async fn an_absent_key_is_rejected_rather_than_read_as_uncapped() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, _) = put_budget(&state, "analyst", json!({})).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "an empty body must never be read as 'remove the cap'"
        );
        assert_eq!(
            team_row(&state, "analyst").await["budgetUsdDaily"],
            5.0,
            "and nothing was written"
        );
    }

    /// A cap has to be a real, non-negative number of dollars — the same rule
    /// the manifest validator applies, so the console cannot store a value
    /// `company.toml` would have rejected.
    #[tokio::test]
    async fn a_nonsensical_cap_is_refused() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, body) = put_budget(&state, "analyst", json!({"budgetUsdDaily": -1.0})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // NaN and ∞ have no JSON literal, so they arrive as raw tokens. Either
        // outcome is a refusal; what must never happen is one being stored,
        // because `spent >= NaN` is false and the cap would enforce nothing
        // while the console rendered it as set.
        for raw in ["{\"budgetUsdDaily\": NaN}", "{\"budgetUsdDaily\": 1e400}"] {
            let request = Request::builder()
                .method("PUT")
                .uri("/api/v1/company/team/analyst/budget")
                .header("cookie", admin_cookie())
                .header("content-type", "application/json")
                .body(Body::from(raw))
                .unwrap();
            let status = router(state.clone())
                .oneshot(request)
                .await
                .unwrap()
                .status();
            assert!(status.is_client_error(), "{raw} → {status}");
        }

        assert_eq!(
            team_row(&state, "analyst").await["budgetUsdDaily"],
            5.0,
            "no refused write left anything behind"
        );
    }

    /// An unknown teammate 404s rather than storing an override nothing reads.
    #[tokio::test]
    async fn an_unknown_teammate_is_not_found() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, _) = put_budget(&state, "nobody", json!({"budgetUsdDaily": 1.0})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = send(
            &state,
            "DELETE",
            "/api/v1/company/team/nobody/budget",
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The privilege boundary: a signed-in **member** cannot change a cap, and
    /// an unauthenticated caller cannot reach the route at all.
    ///
    /// "A cap that can be raised silently is not much of a cap" — so this is the
    /// assertion that makes the enforcement worth having. It is checked on the
    /// backend, never on the console's hidden buttons.
    #[tokio::test]
    async fn a_non_admin_cannot_change_a_cap() {
        use crate::ports::UserRole;

        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let member =
            crate::server::test_support::seed_session(&state, "acme", UserRole::Member).await;

        for (method, body) in [
            ("PUT", Some(json!({"budgetUsdDaily": 999.0}))),
            ("DELETE", None),
        ] {
            let (status, _) = send(
                &state,
                method,
                "/api/v1/company/team/analyst/budget",
                body.clone(),
                Some(&member),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} as a member must be refused"
            );

            let (status, _) = send(
                &state,
                method,
                "/api/v1/company/team/analyst/budget",
                body,
                None,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} with no session must be refused"
            );
        }

        assert_eq!(
            team_row(&state, "analyst").await["budgetUsdDaily"],
            5.0,
            "the manifest cap is untouched"
        );
    }

    /// A teammate created through the console can be given a cap at creation —
    /// and that is admin-only, while a budget-less add stays open to any member
    /// exactly as it was before #343.
    #[tokio::test]
    async fn a_new_teammate_can_be_created_with_a_cap() {
        use crate::ports::UserRole;

        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let member =
            crate::server::test_support::seed_session(&state, "acme", UserRole::Member).await;

        // A member may still add a teammate — no permission was taken away.
        let (status, plain) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Jamie", "role": "Growth"})),
            Some(&member),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{plain}");
        assert!(plain.get("budgetUsdDaily").is_none(), "{plain}");

        // …but not with a budget attached.
        let (status, _) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Sam", "role": "Ops", "budgetUsdDaily": 4.0})),
            Some(&member),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "setting a cap is admin-only wherever it happens"
        );

        // An admin can.
        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Sam", "role": "Ops", "budgetUsdDaily": 4.0})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(created["budgetUsdDaily"], 4.0, "{created}");
        let sam = created["id"].as_str().unwrap().to_string();
        let row = team_row(&state, &sam).await;
        assert_eq!(
            row["budgetUsdDaily"], 4.0,
            "the cap and the teammate landed in one save: {row}"
        );
        assert!(row["budgetSetBy"].is_string(), "{row}");
    }

    /// Issue #1530: a teammate can be born with a persona override — the
    /// create-time path writes it in the same save as the teammate, and the
    /// agent detail reads it back as the effective instructions. Takes no
    /// permission: any member may add a teammate with instructions.
    #[tokio::test]
    async fn add_member_with_instructions_persists_the_override() {
        use crate::ports::UserRole;
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let member =
            crate::server::test_support::seed_session(&state, "acme", UserRole::Member).await;

        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({
                "name": "Jamie",
                "role": "Growth",
                "instructions": "Be terse and data-first."
            })),
            Some(&member),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a member may add with instructions: {created}"
        );
        let jamie = created["id"].as_str().unwrap().to_string();

        // Read the detail back, so this is the stored override rather than the
        // handler's own answer.
        let (status, detail) = send(
            &state,
            "GET",
            &format!("/api/v1/company/team/{jamie}"),
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_eq!(
            detail["instructions"], "Be terse and data-first.",
            "{detail}"
        );
        assert_eq!(detail["instructionsOverridden"], true, "{detail}");
    }

    /// Issue #1674: a setup-created teammate carries its job shape (`focus`) so
    /// it is created with the belt that shape was approved with on the review
    /// screen, rather than inheriting the whole company default. `research` is
    /// the read-only shape: its effective grants hold no `workspace.write`, and
    /// a focus-less add still gets the standard company-wide grant.
    #[tokio::test]
    async fn a_teammate_created_with_a_focus_is_scoped_to_that_focus_belt() {
        use crate::ports::UserRole;
        let home_dir = home();
        let state = state_with_manifest(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[tools]\n\
             allow = [\"workspace.read\", \"workspace.write\", \"docs.*\", \
             \"files.*\", \"web.*\", \"search\", \"mcp:*\"]\n",
        )
        .await;
        let member =
            crate::server::test_support::seed_session(&state, "acme", UserRole::Member).await;

        // A Research teammate: reads the workspace and browses, but has no
        // business writing the company's own guidance tree.
        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({
                "name": "Jamie",
                "role": "Researcher",
                "focus": "research",
            })),
            Some(&member),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let jamie = created["id"].as_str().unwrap().to_string();
        let row = team_row(&state, &jamie).await;
        let grants = |field: &str| {
            row["tools"][field]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let effective = grants("effective");
        assert!(
            effective.contains(&"workspace.read"),
            "research reads the workspace: {effective:?}"
        );
        assert!(
            !effective.contains(&"workspace.write"),
            "research must not write the workspace it reports on: {effective:?}"
        );
        let requested = grants("requested");
        assert!(
            !requested.contains(&"workspace.write"),
            "the stored belt is the research belt, not the company grant: {requested:?}"
        );

        // A focus-less add keeps the standard company-wide grant — the field
        // takes no permission away from the generic add path.
        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Sam", "role": "Generalist"})),
            Some(&member),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let sam = created["id"].as_str().unwrap().to_string();
        let row = team_row(&state, &sam).await;
        let effective = row["tools"]["effective"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(
            effective.contains(&"workspace.write"),
            "a focus-less add still inherits the company grant: {effective:?}"
        );
    }

    /// Issue #788/#789: naming a BYO billing namespace for a NEW teammate is a
    /// billing decision, and a member must not be able to make it. A budget
    /// that spends money is already admin-only; an explicit `chargebee` grant
    /// without a budget must be too — otherwise any member could mint a
    /// billing-capable teammate the day the company ceiling includes one, while
    /// editing an existing teammate's `tools` is already admin-only.
    #[tokio::test]
    async fn a_member_may_not_create_a_teammate_with_a_billing_grant() {
        use crate::ports::UserRole;
        let home_dir = home();
        let state = state_with_manifest(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[tools]\n\
             allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
             \"search\", \"mcp:*\", \"chargebee\"]\n",
        )
        .await;
        let member =
            crate::server::test_support::seed_session(&state, "acme", UserRole::Member).await;

        let (status, body) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Jamie", "role": "Billing", "tools": ["chargebee"]})),
            Some(&member),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

        // The refused add must not have persisted a teammate.
        let (list_status, team) = get_team(&state).await;
        assert_eq!(list_status, StatusCode::OK, "{team}");
        let rows = team.as_array().unwrap();
        assert!(
            rows.iter().all(|r| r["name"] != "Jamie"),
            "a refused add must not persist a teammate: {team}"
        );

        // An admin can still mint the billing-capable teammate.
        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Dana", "role": "Billing", "tools": ["chargebee"]})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(created["id"], "dana", "{created}");
    }

    /// An **overlay** teammate can be capped after the fact too — the case the
    /// pre-#343 read path hardcoded to `None` ("uncapped in v1").
    ///
    /// Capping it also has to start the spend read for the whole roster, which
    /// the old `any_capped` scan (manifest agents only) would have missed on a
    /// company whose only capped teammate came from the console.
    #[tokio::test]
    async fn an_overlay_teammate_can_be_capped_and_reports_its_spend() {
        let home_dir = home();
        let state = state_with_manifest(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n",
        )
        .await;

        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Jamie", "role": "Growth"})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let jamie = created["id"].as_str().unwrap().to_string();

        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: crate::ports::now_millis(),
                    agent: jamie.clone(),
                    provider: "managed".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cost_usd: 0.75,
                    kind: SampleKind::Inference,
                    run_id: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        let (status, _) = put_budget(&state, &jamie, json!({"budgetUsdDaily": 2.0})).await;
        assert_eq!(status, StatusCode::OK);

        let row = team_row(&state, &jamie).await;
        assert_eq!(row["budgetUsdDaily"], 2.0, "{row}");
        assert!(
            (row["spentTodayUsd"].as_f64().unwrap() - 0.75).abs() < 1e-9,
            "capping the only console-added teammate must start the meter read \
             for the roster: {row}"
        );
    }

    /// A budget write answers with the teammate as it **effectively** stands,
    /// not as the blueprint declared it.
    ///
    /// `updated_row` is a second place the roster is rendered, and it used to
    /// read the raw manifest row. Once a manifest teammate became editable that
    /// made one card change identity depending on which route last touched it:
    /// a console rename showed on the Team page and then vanished the moment an
    /// admin set a cap, because the budget response overwrote the row with the
    /// name and role from `company.toml`.
    #[tokio::test]
    async fn a_budget_write_answers_with_the_edited_identity() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        // Rename a blueprint teammate through the console.
        let (status, _) = send(
            &state,
            "PATCH",
            "/api/v1/company/team/analyst",
            Some(json!({"role": "Managing Director", "description": "Runs the place."})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Then set a cap on the same teammate. The response is a roster row.
        let (status, row) = put_budget(&state, "analyst", json!({"budgetUsdDaily": 4.0})).await;
        assert_eq!(status, StatusCode::OK, "{row}");
        assert_eq!(
            row["role"], "Managing Director",
            "the budget write answered with the blueprint's role, undoing the rename on the \
             card the console re-renders from: {row}"
        );
        assert_eq!(row["description"], "Runs the place.", "{row}");

        // And clearing the cap answers the same way — same helper, same defect.
        let (status, cleared) = send(
            &state,
            "DELETE",
            "/api/v1/company/team/analyst/budget",
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{cleared}");
        assert_eq!(cleared["role"], "Managing Director", "{cleared}");
    }

    /// Removing a teammate takes its override with it, so the record does not
    /// accumulate rows for teammates that no longer exist.
    #[tokio::test]
    async fn removing_a_teammate_drops_its_budget_override() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (_, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Jamie", "role": "Growth", "budgetUsdDaily": 2.0})),
            Some(&admin_cookie()),
        )
        .await;
        let jamie = created["id"].as_str().unwrap().to_string();

        let (status, _) = send(
            &state,
            "DELETE",
            &format!("/api/v1/company/team/{jamie}"),
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let record = state
            .registry()
            .get(&CompanyId::new("acme"))
            .unwrap()
            .store()
            .load(&CompanyId::new("acme"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            record.overlay_budgets.is_empty(),
            "the removed teammate's override went with it: {:?}",
            record.overlay_budgets
        );
    }

    /// Issue #686 — a console-added teammate gets a readable snake_case id
    /// derived from its name, so its workspace folder reads
    /// `agents/dana_designer/` rather than `agents/019fad5ada20-…/`.
    ///
    /// A second teammate with the same name suffixes rather than being refused:
    /// duplicate display names were always accepted here, and taking that away
    /// would be a capability regression dressed as a bug fix.
    #[tokio::test]
    async fn a_console_added_teammate_gets_a_readable_id() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let add = async |name: &str| {
            let (status, created) = send(
                &state,
                "POST",
                "/api/v1/company/team",
                Some(json!({"name": name, "role": "Designer"})),
                Some(&admin_cookie()),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{created}");
            created["id"].as_str().unwrap().to_string()
        };

        assert_eq!(add("Dana Designer").await, "dana_designer");
        assert_eq!(add("Dana Designer").await, "dana_designer_2");
        // A name colliding with a manifest agent's id steps past it — an
        // unsuffixed `writer` would be dropped by `build_roster` and the
        // teammate would save without ever materialising.
        assert_eq!(add("Writer").await, "writer_2");
        // A name with no legal stem in it takes the shared fallback.
        assert_eq!(add("24/7").await, "teammate");
    }

    /// The slug is a seat name, not a chain of custody: removing a teammate
    /// frees its id, and re-adding the same name takes it back — which is what
    /// makes remove-plus-re-add the remedy for a typo'd name, since the new
    /// teammate adopts the old `Agents/<slug>/` folder.
    ///
    /// Pinned rather than left implicit because it is the one consequence of
    /// name-derived ids that a generated id did not have.
    #[tokio::test]
    async fn removing_a_teammate_frees_its_slug_for_reuse() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (_, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Dana Designer", "role": "Designer"})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(created["id"], "dana_designer");

        let (status, _) = send(
            &state,
            "DELETE",
            "/api/v1/company/team/dana_designer",
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (_, again) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Dana Designer", "role": "Designer"})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(
            again["id"], "dana_designer",
            "the freed slug comes back rather than suffixing past a ghost: {again}"
        );
    }

    /// Issue #1781 review, Codex P2 follow-up: the sibling of the desk-side
    /// fix — a legacy **overlay** teammate at the literal id `operator`
    /// (grandfathered; `POST .../team` reserves this id going forward, the
    /// same way `create_desk` reserves it for desks) diverts
    /// `operator_feed_channel()` to the fallback address via `is_roster_agent`.
    /// Unlike a manifest teammate, `remove_member`'s overlay branch deletes
    /// outright with no `retire_agent` tombstone — so without this fix the
    /// live `is_roster_agent`/`is_retired` checks would both go false the
    /// moment the delete lands, reverting the feed to `OPERATOR_CHANNEL` and
    /// orphaning every report already journaled under the fallback.
    ///
    /// Seeded directly on the stored record rather than through `POST
    /// .../team`, for the same reason the desk-side test seeds its collision
    /// directly: the creation route has refused this id since before this fix
    /// existed, so this shape can only be reached by data that predates it.
    #[tokio::test]
    async fn removing_an_overlay_teammate_at_the_operator_id_keeps_the_feed_diverted() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        let mut record = runtime.store().load(&id).await.unwrap().unwrap();
        record
            .overlay_agents
            .push(crate::ports::types::OverlayAgent {
                id: "operator".to_string(),
                name: "Legacy Operator".to_string(),
                role: "Chief of Staff".to_string(),
                description: None,
                tools: None,
                model: None,
                harness: None,
            });
        runtime.store().save(&record).await.unwrap();

        let reloaded = runtime.store().load(&id).await.unwrap().unwrap();
        assert_eq!(
            reloaded.operator_feed_channel(),
            crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK,
            "fixture must start in the collision state this test exercises"
        );

        let (status, _) = send(
            &state,
            "DELETE",
            "/api/v1/company/team/operator",
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let after = runtime.store().load(&id).await.unwrap().unwrap();
        assert!(
            !after.is_roster_agent(crate::runtime::channel::OPERATOR_CHANNEL),
            "the colliding teammate must actually be gone, or this is not \
             exercising the live-check-flips-back failure mode at all"
        );
        assert_eq!(
            after.operator_feed_channel(),
            crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK,
            "the feed address must stay on the fallback once the overlay \
             teammate that caused the collision is deleted — flipping back to \
             OPERATOR_CHANNEL would orphan every report already journaled \
             under the fallback and let the deleted teammate's own historical \
             DM history (chat_id == \"operator\") resurface as system-feed \
             content"
        );
    }

    /// The id is minted once. `PATCH …/team/{id}` renames the teammate and
    /// leaves the id alone — a name-keyed id would orphan the teammate's
    /// workspace folder, its budget row and its desk memberships on every
    /// correction (the trap name-keyed DM ids sprang in issue #364).
    #[tokio::test]
    async fn renaming_a_teammate_does_not_remint_its_id() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (_, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Dana Designer", "role": "Designer"})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(created["id"], "dana_designer");

        let (status, edited) = send(
            &state,
            "PATCH",
            "/api/v1/company/team/dana_designer",
            Some(json!({"name": "Dana Diaz"})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{edited}");
        assert_eq!(edited["name"], "Dana Diaz");
        assert_eq!(
            edited["id"], "dana_designer",
            "the id records what the teammate was called at creation: {edited}"
        );

        // And the per-teammate routes still answer on the original slug.
        let (status, _) = put_budget(&state, "dana_designer", json!({"budgetUsdDaily": 3.0})).await;
        assert_eq!(status, StatusCode::OK);
        let row = team_row(&state, "dana_designer").await;
        assert_eq!(row["budgetUsdDaily"], 3.0, "{row}");
        assert_eq!(row["name"], "Dana Diaz", "{row}");
    }

    /// Every teammate route keyed on `{agent_id}` keeps working when that id is
    /// a slug — the inbox toggle alongside the budget pair, since the slug now
    /// travels in a URL path where a generated id used to.
    #[tokio::test]
    async fn slug_ids_work_across_the_per_teammate_routes() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (_, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Ana Maria (Growth)", "role": "Growth"})),
            Some(&admin_cookie()),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        assert_eq!(id, "ana_maria_growth");

        let (status, _) = send(
            &state,
            "PUT",
            &format!("/api/v1/company/team/{id}/inbox"),
            Some(json!({"enabled": true})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(team_row(&state, &id).await["inboxEnabled"], true);

        let (status, _) = put_budget(&state, &id, json!({"budgetUsdDaily": 1.5})).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            &state,
            "DELETE",
            &format!("/api/v1/company/team/{id}/budget"),
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            team_row(&state, &id).await.get("budgetUsdDaily").is_none(),
            "the reset came back through the slug-keyed route"
        );
    }

    /// Issue #304 — the cap was never on the wire at all, so the issue's "and
    /// displayed in the console" was stale against main. A capped teammate now
    /// carries both its cap and its spend since UTC midnight, summed from the
    /// meter; an uncapped one carries neither key.
    ///
    /// The omission is the contract, not an optimisation: the console tells
    /// "spends freely" from "capped and has spent nothing" by presence alone.
    #[tokio::test]
    async fn a_capped_teammate_carries_its_cap_and_todays_spend() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let now = crate::ports::now_millis();
        for cost in [1.25f64, 0.50] {
            runtime
                .usage()
                .record(
                    &id,
                    &UsageSample {
                        at_millis: now,
                        agent: "analyst".into(),
                        provider: "managed".into(),
                        input_tokens: 10,
                        output_tokens: 5,
                        cached_input_tokens: 0,
                        cost_usd: cost,
                        kind: SampleKind::Inference,
                        run_id: None,
                        model: None,
                    },
                )
                .await
                .unwrap();
        }
        // The uncapped teammate's spend must not leak onto the capped one.
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: now,
                    agent: "writer".into(),
                    provider: "managed".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cost_usd: 9.00,
                    kind: SampleKind::Inference,
                    run_id: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        let (status, body) = get_team(&state).await;
        assert_eq!(status, StatusCode::OK);
        let rows = body.as_array().unwrap();

        let analyst = rows.iter().find(|m| m["id"] == "analyst").unwrap();
        assert_eq!(analyst["budgetUsdDaily"], 5.0, "{analyst}");
        assert!(
            (analyst["spentTodayUsd"].as_f64().unwrap() - 1.75).abs() < 1e-9,
            "spend is summed per agent since UTC midnight: {analyst}"
        );

        let writer = rows.iter().find(|m| m["id"] == "writer").unwrap();
        assert!(
            writer.get("budgetUsdDaily").is_none() && writer.get("spentTodayUsd").is_none(),
            "an uncapped teammate omits both keys: {writer}"
        );
    }

    /// Yesterday's spend is not today's: the read is anchored at 00:00 UTC, the
    /// same boundary the harness gate and the policy arm enforce against.
    #[tokio::test]
    async fn spend_today_excludes_yesterday() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let yesterday =
            crate::metering::utc_day_start_millis(crate::ports::now_millis()).saturating_sub(1);
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: yesterday,
                    agent: "analyst".into(),
                    provider: "managed".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cost_usd: 9.00,
                    kind: SampleKind::Inference,
                    run_id: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        let (status, body) = get_team(&state).await;
        assert_eq!(status, StatusCode::OK);
        let analyst = body
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "analyst")
            .unwrap()
            .clone();
        assert_eq!(analyst["budgetUsdDaily"], 5.0, "{analyst}");
        assert_eq!(
            analyst["spentTodayUsd"], 0.0,
            "a capped teammate with no spend today reads $0, not yesterday's $9: {analyst}"
        );
    }

    /// A company that caps nobody renders exactly as it did before #304 — and
    /// the meter is never consulted for it.
    #[tokio::test]
    async fn an_uncapped_company_is_unchanged() {
        let home_dir = home();
        let state = state_with_manifest(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n",
        )
        .await;

        let (status, body) = get_team(&state).await;
        assert_eq!(status, StatusCode::OK);
        let writer = &body.as_array().unwrap()[0];
        assert_eq!(writer["id"], "writer");
        assert!(
            writer.get("budgetUsdDaily").is_none() && writer.get("spentTodayUsd").is_none(),
            "{writer}"
        );
    }

    // --- Declared tier on the roster list (issue #643) -----------------------

    /// A roster whose three teammates each answer the tier question
    /// differently: `ceo` is tagged as the orchestrator, `writer` declares a
    /// *non*-orchestrator tier, and `intern` declares nothing at all.
    const TIERED_ROSTER: &str = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief Executive\"\ntier = \"orchestrator\"\n\
         [[agent]]\nid = \"writer\"\nrole = \"Writer\"\ntier = \"reasoning\"\n\
         [[agent]]\nid = \"intern\"\nrole = \"Intern\"\n";

    /// One agent from `GET …/team/{id}` — the detail read, for cross-checking
    /// that the list has not grown a second opinion.
    async fn agent_detail_row(state: &AppState, agent: &str) -> Value {
        let (status, body) = send(
            state,
            "GET",
            &format!("/api/v1/company/team/{agent}"),
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    /// Issue #643 — the declared tier reaches the roster list verbatim.
    ///
    /// The list carried no tier at all, so the overview graph (built from this
    /// read) stamped a literal `worker` on every node: a company declaring
    /// `tier = "orchestrator"` read back as a worker on its own graph.
    ///
    /// The **undeclared** teammate is the half that keeps the fix honest. Its
    /// row must omit the key entirely — not `"worker"`, not `null` — because
    /// absence is the only wire shape that says "this company declares no tier
    /// here" rather than asserting one on its behalf.
    #[tokio::test]
    async fn the_roster_list_carries_each_declared_tier_verbatim() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), TIERED_ROSTER).await;

        let ceo = team_row(&state, "ceo").await;
        assert_eq!(ceo["tier"], "orchestrator", "{ceo}");
        assert_eq!(ceo["isOrchestrator"], true, "{ceo}");

        // A declared tier that is not the orchestrator tier: carried verbatim,
        // and it does not make the teammate the orchestrator.
        let writer = team_row(&state, "writer").await;
        assert_eq!(writer["tier"], "reasoning", "{writer}");
        assert_eq!(
            writer["isOrchestrator"], false,
            "a declared tier is a hint, not the roster rule: {writer}"
        );

        // The negative control: undeclared means no key.
        let intern = team_row(&state, "intern").await;
        assert!(
            intern.get("tier").is_none(),
            "an undeclared tier omits the key — a defaulted \"worker\" here is \
             indistinguishable from a declaration and is the whole of #643: {intern}"
        );
        assert_eq!(intern["isOrchestrator"], false, "{intern}");

        // No row anywhere invents the literal the graph used to print.
        let (_, all) = get_team(&state).await;
        for row in all.as_array().unwrap() {
            assert_ne!(row["tier"], "worker", "nobody declared \"worker\": {row}");
        }

        // And the list agrees with the detail read, which is the property the
        // shared helpers exist to make unrepresentable rather than merely true.
        for id in ["ceo", "writer", "intern"] {
            let (list, detail) = (
                team_row(&state, id).await,
                agent_detail_row(&state, id).await,
            );
            assert_eq!(
                list.get("tier"),
                detail.get("tier"),
                "{id}: {list} {detail}"
            );
            assert_eq!(
                list["isOrchestrator"], detail["isOrchestrator"],
                "{id}: {list} {detail}"
            );
        }
    }

    /// A company that tags nobody still has an orchestrator: the first declared
    /// agent, by the same roster rule the harness resolves with.
    ///
    /// This is the case a console that re-derived the marker from the tier
    /// string would get wrong — and get wrong invisibly, since an untagged CEO
    /// draws as an ordinary worker rather than as an error.
    #[tokio::test]
    async fn an_untagged_roster_still_names_an_orchestrator_on_the_list() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let analyst = team_row(&state, "analyst").await;
        assert_eq!(
            analyst["isOrchestrator"], true,
            "the first declared agent is the orchestrator when nobody is tagged: {analyst}"
        );
        assert!(
            analyst.get("tier").is_none(),
            "…and it says so without inventing a tier for them: {analyst}"
        );

        // The negative control: exactly one, and it is the first.
        let writer = team_row(&state, "writer").await;
        assert_eq!(writer["isOrchestrator"], false, "{writer}");
    }

    /// An overlay teammate has no manifest row, so it declares no tier and the
    /// roster rule never picks it — even on a company whose manifest roster is
    /// empty, where "the first declared agent" names nobody at all.
    #[tokio::test]
    async fn an_overlay_teammate_declares_no_tier_and_is_not_the_orchestrator() {
        let home_dir = home();
        let state = state_with_manifest(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Nova", "role": "Researcher"})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert!(
            created.get("tier").is_none() && created["isOrchestrator"] == false,
            "the create response answers both the same way the reads do: {created}"
        );

        let id = created["id"].as_str().unwrap().to_string();
        let row = team_row(&state, &id).await;
        assert!(
            row.get("tier").is_none(),
            "an overlay teammate has no `[[agent]]` row to declare a tier: {row}"
        );
        assert_eq!(
            row["isOrchestrator"], false,
            "an empty manifest roster names nobody, so it does not fall through \
             to the overlay half: {row}"
        );
    }

    // --- Baseline provenance, and the first-run gate (issue #1404) ----------

    /// The roster says which of its rows came from the global baseline.
    ///
    /// This is the field the console's first-run gate turns on. `apply_globals`
    /// appends `globals/agents/*.toml` to **every** company whatever its
    /// manifest says, so a company nobody has ever staffed still answers this
    /// route with a non-empty list — and "is the roster empty?" therefore
    /// answered `no` everywhere, which is what made first-run setup unreachable
    /// in the shipped product.
    ///
    /// Asserted as "at least one row, all of them global" rather than against a
    /// count or the four current ids: the baseline is meant to grow, and a test
    /// that pins its contents here would fail for the wrong reason.
    #[tokio::test]
    async fn a_company_with_no_declared_roster_answers_with_the_baseline_only() {
        let home_dir = home();
        let state = state_with_globals(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (status, body) = get_team(&state).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let rows = body.as_array().unwrap();
        assert!(
            !rows.is_empty(),
            "the baseline is merged into every company, so this is never empty: {body}"
        );
        assert!(
            rows.iter().all(|row| row["global"] == true),
            "a company declaring no `[[agent]]` has nothing but baseline \
             teammates, and every one of them must say so: {body}"
        );
    }

    /// A teammate the company wrote, and one the operator adds, are both
    /// `global: false` — beside a baseline that is `true` on the same read.
    ///
    /// Both halves are the point. The gate must stay shut for a company that
    /// shipped with a roster (`docs/spec/runtime/company-setup.md`), and it must
    /// close the moment setup creates the first teammate.
    #[tokio::test]
    async fn a_declared_or_operator_added_teammate_is_never_marked_global() {
        let home_dir = home();
        let state = state_with_globals(home_dir.path(), ROSTER).await;

        let declared = team_row(&state, "analyst").await;
        assert_eq!(
            declared["global"], false,
            "a `[[agent]]` the company wrote is the company's own: {declared}"
        );

        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Nova", "role": "Researcher"})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(
            created["global"], false,
            "the create response answers the same way the read does: {created}"
        );
        let id = created["id"].as_str().unwrap().to_string();
        assert_eq!(team_row(&state, &id).await["global"], false);

        // …and the baseline on the same roster still says otherwise, so the two
        // are distinguishable rather than uniformly false.
        let (_, body) = get_team(&state).await;
        assert!(
            body.as_array()
                .unwrap()
                .iter()
                .any(|row| row["global"] == true),
            "the baseline rows are on this roster too: {body}"
        );
    }

    /// A baseline teammate — a blueprint row like any other, merged into every
    /// company — can be deleted, and stays deleted across a reload. It is a
    /// tombstone rather than a manifest rewrite, so this is the assertion that
    /// says the blueprint being re-read on every load does not resurrect it.
    #[tokio::test]
    async fn a_baseline_teammate_can_be_deleted_and_stays_deleted() {
        let home_dir = home();
        let state = state_with_globals(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (_, before) = get_team(&state).await;
        let before = before.as_array().unwrap().clone();
        assert!(
            before.len() > 1,
            "the baseline seeds more than one teammate: {before:?}"
        );
        let id = before[0]["id"].as_str().unwrap().to_string();

        let (status, _) = send(
            &state,
            "DELETE",
            &format!("/api/v1/company/team/{id}"),
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (_, after) = get_team(&state).await;
        let after = after.as_array().unwrap();
        assert_eq!(after.len(), before.len() - 1, "{after:?}");
        assert!(
            !after.iter().any(|row| row["id"] == id.as_str()),
            "the blueprint still declares it, so a re-read must not bring it \
             back: {after:?}"
        );
    }

    /// The one refusal the roster keeps: a company must not be left with nobody
    /// on it. Without this the console could empty the roster entirely, which
    /// has no orchestrator, nobody to answer a message, and no way back.
    #[tokio::test]
    async fn the_last_teammate_cannot_be_deleted() {
        let home_dir = home();
        let state = state_with_globals(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        // Delete every teammate but one, which must succeed all the way down.
        let (_, body) = get_team(&state).await;
        let ids: Vec<String> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_string())
            .collect();
        for id in &ids[..ids.len() - 1] {
            let (status, _) = send(
                &state,
                "DELETE",
                &format!("/api/v1/company/team/{id}"),
                None,
                Some(&admin_cookie()),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "removing {id}");
        }

        let last = ids.last().unwrap();
        let (status, refusal) = send(
            &state,
            "DELETE",
            &format!("/api/v1/company/team/{last}"),
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{refusal:?}");

        let (_, after) = get_team(&state).await;
        assert_eq!(after.as_array().unwrap().len(), 1, "{after}");
    }
}
