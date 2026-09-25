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
        // Issue #1989: designs a WHOLE teammate from a name and a sentence, for
        // the reduced Add-teammate dialog. A static segment beside `/team/draft`
        // and for the same reason — nothing serves `POST` on `/team/{agent_id}`,
        // so this cannot be confused with a teammate whose id is `design`.
        //
        // Deliberately id-less: this is the only pass that may write a `role`,
        // and taking no agent id is what makes it structurally unable to rewrite
        // an existing teammate's. See `design_teammate`.
        .merge(scoped(
            "/team/design",
            post(super::team_agent::design_teammate),
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
    /// Which `[[harness]]` this teammate runs its turns on, by declared id —
    /// the same field, from the same helper, as `GET …/team/{agent_id}`.
    ///
    /// Absent means the harness marked `default = true`, **not** "no harness":
    /// every teammate resolves to one. Skipped rather than defaulted for
    /// `tier`'s reason — a default is indistinguishable from a declaration on
    /// the wire, and a roster card that named the default as though this
    /// teammate had pinned it would be claiming something the record does not
    /// say.
    #[serde(skip_serializing_if = "Option::is_none")]
    harness: Option<String>,
    /// This teammate's own model pin: the hint forwarded to an ACP harness, or
    /// the model half of its `{provider, model}` pair on a built-in one.
    ///
    /// Absent means it declares none and inherits the company default. Carried
    /// on the list for the reason `tier` and `desks` are: the roster grid draws
    /// a card per teammate, and a field the list omitted was a field the card
    /// had to invent or leave blank — with no way to resolve it short of an
    /// N+1 over the detail read.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// The provider half of this teammate's own `{provider, model}` pair, set
    /// only together with [`model`](Self::model) and only meaningful on a
    /// built-in harness. Absent means the company default.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
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
    /// The desks this teammate may hand work to (`[[agent]].delegates_to`), as
    /// declared — `["*"]` meaning every desk.
    ///
    /// This is the company's **delegation address space**: the edge set a
    /// teammate could traverse, as opposed to the ones it has. Carried on the
    /// roster read for the same reason `desks` and `tier` are — the console's
    /// graph is built from this list, and a field the list omits is a field the
    /// graph has to invent. Without it a comms graph can only draw traffic that
    /// has already happened, so a company that has not run yet draws as a set of
    /// unconnected agents, which is not what its manifest says.
    ///
    /// Omitted when empty: a teammate that delegates to nothing is the default,
    /// and an empty array on every row is noise on the wire.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    delegates_to: Vec<String>,
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
        // Through `team_agent`'s helpers, never recomputed here: the
        // roster list and the detail read must not be able to disagree about
        // the same teammate (issues #264, #601, #643). A second copy of the
        // orchestrator rule in particular would be a copy of a rule that has
        // two arms, and the arm it dropped would be invisible on screen.
        tier: super::team_agent::declared_tier(record, agent_id),
        harness: super::team_agent::declared_harness(record, agent_id),
        model: super::team_agent::declared_model(record, agent_id),
        provider: super::team_agent::declared_provider(record, agent_id),
        is_orchestrator: super::team_agent::is_orchestrator(record, agent_id),
        tools: super::team_agent::agent_tools(record, agent_id),
        desks: super::team_agent::desks_for(record, agent_id),
        // Read off the effective agent, so an overlay teammate and a manifest
        // one answer the same way.
        delegates_to: record
            .effective_agents()
            .into_iter()
            .find(|agent| agent.id == agent_id)
            .map(|agent| agent.delegates_to)
            .unwrap_or_default(),
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
    // The blank-field gap, closed (issue #1989). This was the ONE write path in
    // the repository that stored a teammate's `name` and `role` exactly as they
    // arrived: `PATCH {scope}/team/{agent_id}` refuses a blank one through
    // `trimmed_field`, the orchestrator's `add_agent` tool refuses one,
    // `company.toml` refuses one and `agents/<id>.toml` refuses one — and this
    // route accepted `{"name": "", "role": ""}` with a `200`.
    //
    // What that produced is not a tidiness complaint. `persona_prompt`
    // (`src/company/prompt.rs`) interpolates the role UNGUARDED while the
    // description and instructions blocks beside it are blank-guarded, so a
    // blank role ships the teammate a system prompt reading "You are Dana, the
    //  at Acme."; the orchestrator's Team block and the auto-responder's
    // channel-member block both render `id — role`, so delegation is grounded
    // on a dash; and the detail page's copilot disables itself on a blank role,
    // which is the page the console's create flow lands on. A console-side
    // check is not a substitute for this one — the wire is open to anything
    // holding a session, and the invariant belongs where the record is written.
    //
    // Before the authority check below rather than after, unlike
    // `edit_agent`'s deliberate existence-then-authority ordering: there is no
    // resource to confirm or deny the existence of here, so nothing is
    // disclosed by answering "this request is malformed" first, and a
    // request that cannot be stored should not first cost a permission lookup.
    let name = required_field(&body.name, "name").map_err(|e| e.into_response())?;
    let role = required_field(&body.role, "role").map_err(|e| e.into_response())?;

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
        provider: None,
        // A readable id derived from the name, unique against the roster this
        // record already holds (issue #686). Minted here rather than pushed and
        // renamed later: the id names the teammate's `agents/<id>/` folder and
        // stamps every artifact it authors, so it has to be right on the first
        // save. The surrounding write lock is what makes the uniqueness check
        // and the save below one atomic step.
        id: record.mint_agent_id(&name),
        name,
        role,
        description: body.description,
        // Issue #661 / L5: the teammate's own grant, intersected with the
        // company allow-list by the shared reads/roster build. A teammate created
        // with no stated (and no billing-narrowed) grant is stored as `None` —
        // inherit the company's standard grant — not `Some(vec![])`, which since
        // issue #1804 is an explicit deny-all. This create path expresses only
        // "inherit" and "narrow"; the deny-all state is reachable by editing the
        // teammate afterwards (`PATCH …/team/{id}` with `tools: []`).
        tools: if tools.is_empty() { None } else { Some(tools) },
        skills: None,
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
    // The audit row for a teammate coming into existence.
    //
    // The orchestrator's `add_agent` tool journals the identical variant, and
    // that symmetry is the point: two creation paths that answer "was a teammate
    // added" differently is how the gap this closes opened in the first place.
    //
    // Best-effort — the teammate is already durable, and a journal that refuses
    // the row must not turn a completed mint into a failed request.
    if let Err(err) = company
        .runtime
        .events()
        .append(
            company.id(),
            crate::ports::types::CompanyEvent::TeammateAdded {
                agent_id: agent.id.clone(),
                role: agent.role.clone(),
                // An operator did this from the console, so no agent authored it.
                by_agent_id: None,
                by: company.actor.clone(),
            },
        )
        .await
    {
        tracing::warn!(error = %err, "teammate-added audit row could not be journaled");
    }
    // A brand-new overlay teammate has no `[[agent]]` row at all, so it declares
    // no tier, holds the company's standard grant, and sits on no desk until
    // somebody adds it to one. Resolved through the shared helpers rather than
    // written out here, so this response cannot drift from the two reads
    // (issues #601, #643).
    let tier = super::team_agent::declared_tier(&record, &agent.id);
    let harness = super::team_agent::declared_harness(&record, &agent.id);
    let model = super::team_agent::declared_model(&record, &agent.id);
    let provider = super::team_agent::declared_provider(&record, &agent.id);
    let is_orchestrator = super::team_agent::is_orchestrator(&record, &agent.id);
    let tools = super::team_agent::agent_tools(&record, &agent.id);
    let desks = super::team_agent::desks_for(&record, &agent.id);
    Ok(Json(TeamMemberDto {
        id: agent.id,
        name: Some(agent.name),
        role: agent.role,
        description: agent.description,
        tier,
        harness,
        model,
        provider,
        is_orchestrator,
        tools,
        desks,
        // A console-created teammate delegates nowhere until somebody says so:
        // `delegates_to` is a manifest field and the overlay carries none.
        delegates_to: Vec::new(),
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

/// A required create-time field, trimmed, refusing a blank one (issue #1989).
///
/// Deliberately the same refusal and the same wording as `trimmed_field` in
/// `team_agent.rs`, which is what `PATCH {scope}/team/{agent_id}` applies to the
/// same two fields — a teammate that cannot be *edited* into a blank name or
/// role must not be *born* with one, and an operator who meets both routes
/// should meet one sentence. It is a separate function rather than a shared one
/// because the shapes differ: `PATCH` takes `Option<&str>` where absent means
/// leave-alone, and at creation there is nothing to leave alone — these fields
/// are required by `AddMember` itself, so only their emptiness is in question.
fn required_field(value: &str, field: &str) -> Result<String, ApiError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "a teammate's {field} can't be empty."
        ))));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
#[path = "team_a_member_may_not_tests.rs"]
mod tests_a_member_may_not;
#[cfg(test)]
#[path = "team_an_admin_can_set_tests.rs"]
mod tests_an_admin_can_set;
#[cfg(test)]
#[path = "team_an_uncapped_company_is_tests.rs"]
mod tests_an_uncapped_company_is;
