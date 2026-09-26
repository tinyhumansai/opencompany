//! One agent, opened: the detail read and the edit behind `GET`/`PATCH
//! {scope}/team/{agent_id}` (issue #264).
//!
//! Before this, an agent was a dead end. `GET …/team` returned a name, a role
//! and a description; nothing carried the agent's tier, its tool grants or its
//! desks, and there was no per-agent route at all. So the console could show a
//! card and offer to delete it, and that was the whole of what an operator
//! could learn or do. Worse than the missing screen: **checking what tools a
//! company actually grants an agent had no read surface anywhere**, which is
//! why a tool-grant change could not be verified from outside the process.
//!
//! ## Effective, not declared
//!
//! [`AgentToolsDto`] carries the three levels rather than one, because the
//! interesting number is the one nobody could see. `requested` is what the
//! `[[agent]].tools` line asks for, `companyAllow` is the `[tools].allow`
//! ceiling it is intersected with, and `effective` is what the agent actually
//! ends up holding. An agent that requests `workspace.read` under a company
//! that allows only `composio` requests one tool and holds none, and a surface
//! that printed the request alone would report the opposite of the truth.
//! A `deskCeilingActive` flag sits alongside the desk level so a reader can
//! tell "no desk narrows anything" from "a desk narrows everything away" —
//! the narrowed `deskAllow` list can be empty in both cases.
//!
//! `effective` is computed by
//! [`agent_effective_grants`](crate::runtime::builder::agent_effective_grants)
//! — the *same* function the harness calls when it builds the agent, not a
//! re-implementation of the rule. A second copy would eventually disagree, and
//! a tool-grant readout that disagrees with the harness is worse than none.
//!
//! ## What may be edited, and why that is the line
//!
//! The console edits what the console owns.
//!
//! An **overlay** teammate — one an operator defined through "Define an agent",
//! or the orchestrator created with `add_agent` — lives on the
//! [`CompanyRecord`], which this process writes. Its name, role and description
//! are editable here, and that is the whole of #264's "the roster is write-once
//! per member" complaint: before this, iterating on an agent's instructions
//! meant deleting it and starting over.
//!
//! A **manifest** teammate is declared in the version-controlled `company.toml`
//! — including every teammate from the global baseline, which is merged into
//! *every* company's roster. It used to be uneditable here, which meant the
//! agents a company actually ships with were the ones an operator could never
//! change: a hosted tenant has no `company.toml` to edit and nothing to
//! redeploy, so "edit it in the blueprint" was advice with no action behind it.
//!
//! It is editable now, through an
//! [`AgentOverride`](crate::ports::types::AgentOverride) layered on the record —
//! the shape #343 already used for the one field that was always editable, the
//! daily budget. Nothing rewrites `company.toml`: the blueprint keeps stating
//! what the company launched with, the overlay states what the operator has
//! since decided, and [`CompanyRecord::effective_agent`] is the single place the
//! two are resolved, so the console card and the built roster cannot disagree.
//! The merge is per field, so a field nobody edited still tracks the blueprint
//! across a redeploy.
//!
//! Removal works the same way — `DELETE …/team/{agent_id}` in
//! [`super::team`] records a tombstone rather than rewriting the blueprint —
//! and its only refusal is the company's last teammate.
//!
//! `tier` is read-only for both kinds: it has no override layer, and inventing
//! one is a policy decision rather than something to smuggle into a detail
//! view. `tools` is editable but admin-only, and can only ever *narrow* a
//! teammate within the company grant — see [`edit_agent`].
//!
//! The server states the rule rather than leaving the console to re-derive it:
//! every detail response carries an [`editable`](AgentDetailDto::editable) list,
//! and the console renders a field read-only exactly when the host says it is.
//! A console that decided this for itself would drift from what the host
//! actually accepts, and the operator would meet the disagreement as a failed
//! save.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{self, MethodRouter};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::ACP_AGENTS;
use crate::company::profile_draft::{
    CopilotTurn, DesignedTeammate, DraftRefusal, ProfileDraft, ProfileField, ProfileSubject,
    Sibling, TurnRole, clamp_conversation, clamp_design_brief,
};
use crate::company::setup::clamp_description;
use crate::error::OpenCompanyError;
use crate::ports::store::company_write_lock;
use crate::ports::types::{AgentOverride, CompanyRecord};
use crate::runtime::builder::agent_scoped_grants;
use crate::server::error::ApiError;
use crate::server::ops::ScopedCompany;
use crate::server::ops::team::{AgentPath, daily_spend_samples, double_option};
use crate::server::users::admin::require_admin;

/// The `{scope}/team/{agent_id}` fragment: read one agent, edit one agent.
///
/// Merged into [`super::team::router`]'s existing `/team/{agent_id}` entry
/// rather than declared as its own route — axum panics on two routers claiming
/// one path, even for disjoint methods.
pub(super) fn method_router() -> MethodRouter<AppState> {
    routing::get(agent_detail).patch(edit_agent)
}

/// Which half of the roster a teammate comes from, and therefore what may be
/// done to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum AgentSource {
    /// Declared in the version-controlled `company.toml`.
    Manifest,
    /// Added at runtime by an operator or the orchestrator, stored on the
    /// company record.
    Overlay,
}

/// The fields a `PATCH` accepts for a teammate — manifest-declared or overlay
/// alike. Sent to the console so it renders the same rule the host enforces.
///
/// Grown from `feat/external-acp` meeting #1530 on `main`: that issue added
/// `instructions` and widened this list from overlay-only to both kinds,
/// #1245's harness-picker follow-up added `model` and `harness`, and the keys
/// rework (issue #2306, slice 3a) added `provider` alongside `model` for its
/// `{provider, model}` pair on a `built_in` harness. None of these knew about
/// the others, so this is their union. It widens nothing on its own — `tools`,
/// `model`, `harness` and `provider` stay admin-gated in [`edit_agent`], and
/// [`EDITABLE_FIELDS_MEMBER`] is unchanged from what #1530 left it.
const EDITABLE_FIELDS: [&str; 13] = [
    "name",
    "role",
    "description",
    "tools",
    "instructions",
    "avatar",
    "mascotMode",
    "mascotCostume",
    "mascotSkinColor",
    "mascotHandColor",
    "model",
    "harness",
    "provider",
];

/// The subset a **non-admin** member may `PATCH` (issue #619).
///
/// `tools` is admin-only because an empty list means "the company's standard
/// grant", which makes a `tools` edit a potential *widening* — see
/// [`edit_agent`]. The list is actor-dependent for the reason the module note
/// gives: a console renders a field read-only exactly when the host says it is,
/// so offering `tools` to a member who would meet a `403` on save is precisely
/// the drift `editable` exists to remove.
const EDITABLE_FIELDS_MEMBER: [&str; 9] = [
    "name",
    "role",
    "description",
    "instructions",
    "avatar",
    "mascotMode",
    "mascotCostume",
    "mascotSkinColor",
    "mascotHandColor",
];

/// One agent, in full — everything #264 lists as unreachable.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentDetailDto {
    id: String,
    /// Absent for a manifest teammate, which is named by its role. Same rule as
    /// `GET …/team`.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    role: String,
    /// What the agent was defined with. This is the text that frames the
    /// agent's persona for every turn it takes, which is what the issue means
    /// by "the `AGENT.md` or similar file for that agent" — the manifest
    /// already carries it, the console just never showed it after creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    source: AgentSource,
    /// The field names a `PATCH` will accept for this teammate. Since #1530 a
    /// manifest teammate is no longer empty — it accepts `instructions`, which
    /// write to the override record rather than `company.toml`.
    editable: Vec<&'static str>,
    /// The persona instructions **in force** for this teammate (issue #1530):
    /// an operator override when one is set, else the manifest `prompt`, else
    /// absent. This is what actually frames the agent's turns — the value the
    /// console shows in the editor.
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    /// The manifest `prompt` seed, when this teammate has one — what
    /// "Reset to blueprint" restores. Absent for an overlay teammate (no
    /// manifest row) and for a manifest agent that declares no `prompt`. The
    /// console previews/labels the reset target from this.
    #[serde(skip_serializing_if = "Option::is_none")]
    blueprint_instructions: Option<String>,
    /// Whether an operator override is currently masking the blueprint — the
    /// signal the console gates "Reset to blueprint" on, so it offers the reset
    /// only when there is something to reset to.
    instructions_overridden: bool,
    /// The declared cognition-tier hint, when the manifest sets one. An overlay
    /// teammate has none by construction.
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<String>,
    /// Which `[[harness]]` this teammate runs its turns on, by declared id
    /// (issue #1245's harness-picker follow-up). `None` means the harness
    /// marked `default = true` — read `GET {scope}/harnesses` for the full
    /// declared set, including which one that is.
    #[serde(skip_serializing_if = "Option::is_none")]
    harness: Option<String>,
    /// This teammate's own model, in one of two unrelated meanings depending
    /// on the harness it is bound to: on an **ACP** harness (issue #1245's
    /// per-agent follow-up) it is the model hint forwarded to it, shown as
    /// informational since this response does not itself say which harness
    /// that is; on a **built_in** harness it is the model half of this
    /// teammate's own `{provider, model}` pair (keys rework, issue #2306,
    /// slice 3a), set only together with [`provider`](Self::provider).
    /// Absent means the teammate uses the company default.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// The provider half of this teammate's own `{provider, model}` pair
    /// (keys rework, issue #2306, slice 3a). Set only together with `model`,
    /// and only meaningful on a `built_in` harness — refused on `acp`, where
    /// `model` keeps its ACP meaning instead. Absent means the teammate uses
    /// the company default.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    /// Whether this teammate is the company's orchestrator — resolved by the
    /// roster rule (tagged tier first, else the first declared agent), not read
    /// off `tier` alone, so an untagged roster's real orchestrator is named.
    is_orchestrator: bool,
    tools: AgentToolsDto,
    desks: Vec<AgentDeskDto>,
    inbox_enabled: bool,
    /// The face this teammate wears, when somebody has chosen one — the same
    /// field, resolved through the same record helper, as `GET …/team`
    /// (`docs/spec/runtime/avatars.md`). Absent means nobody has chosen and the
    /// console draws the mascot it hashes from the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar: Option<String>,
    /// Whether this teammate's `mascot:animated` canvas plays, when somebody
    /// has chosen a mode (`docs/spec/runtime/avatars.md`). Only meaningful
    /// when `avatar` is `"mascot:animated"`. Absent means the file's own
    /// default mode (`"animated"`), not "no mascot".
    #[serde(skip_serializing_if = "Option::is_none")]
    mascot_mode: Option<String>,
    /// The mascot costume this teammate wears, when somebody has chosen one.
    /// Applies whichever mode is in force. Absent means the file's own
    /// default costume.
    #[serde(skip_serializing_if = "Option::is_none")]
    mascot_costume: Option<String>,
    /// The mascot's skin (body) color, when somebody has chosen one. Absent
    /// means the file's own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    mascot_skin_color: Option<String>,
    /// The mascot's hand/accent color, when somebody has chosen one. Absent
    /// means the file's own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    mascot_hand_color: Option<String>,
    /// The cap in force, its spend, and its attribution — the same fields and
    /// the same absent-means-uncapped contract as `GET …/team`.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_usd_daily: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    spent_today_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_set_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_set_at_millis: Option<u64>,
}

/// An agent's tool grants at all three levels, so the resolution is legible
/// rather than asserted.
///
/// Built **only** through [`agent_tools`], so every surface that renders an
/// agent's tools renders the same list — see that function for why that is a
/// rule rather than a convenience.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentToolsDto {
    /// The grant the agent asks for, in its three representable states (issue
    /// #1804): `null` = **inherit** the company's standard grant, `[]` = an
    /// **explicit no-tools** grant (deny-all), `[globs]` = **narrow**. The
    /// console renders all three distinctly and lets an admin set each — before
    /// #1804 an empty list was ambiguous between "standard" and "nothing", and
    /// this field could not tell them apart.
    requested: Option<Vec<String>>,
    /// The company-wide `[tools].allow` ceiling.
    company_allow: Vec<String>,
    /// The ceiling contributed by the desks this agent sits on — the union of
    /// their `tools`, already narrowed by `company_allow`.
    ///
    /// **Empty means the narrowed ceiling grants nothing**, which is *not* the
    /// same as "no desk narrows anything" — see `desk_ceiling_active`. A desk
    /// ceiling can resolve to an empty list while still being active (its only
    /// grant is an explicit opt-in the company's bare `*` does not confer), and
    /// the console has to tell those apart or it substitutes `company_allow`
    /// and promises grants the host drops. It is empty for every company that
    /// has not set a desk ceiling, which is most of them.
    desk_allow: Vec<String>,
    /// Whether any desk this agent sits on states a `tools` ceiling — distinct
    /// from `desk_allow`, which is that ceiling *narrowed by the company grant*
    /// and can legitimately resolve to empty. This is the sentinel the console
    /// preview keys on: `true` means the desk level is in play even when the
    /// narrowed list is empty.
    desk_ceiling_active: bool,
    /// What the agent actually holds, after all three levels.
    effective: Vec<String>,
}

/// A desk this agent sits on.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentDeskDto {
    id: String,
    name: String,
    /// Whether this agent is the desk's lead — the first effective member, who
    /// receives a `delegate_to_desk` hand-off.
    lead: bool,
}

/// The tool globs an agent *asks* for, resolved identically for every reader.
///
/// A manifest teammate's `[[agent]].tools` line, or — for an overlay teammate —
/// its own [`OverlayAgent::tools`](crate::ports::types::OverlayAgent::tools)
/// grant (issue #661 / L5), which mirrors `harness::overlay_agent_to_manifest`.
///
/// Returns the field's three-state value verbatim (issue #1804): `None` =
/// **inherit** the company's standard grant, `Some(vec![])` = an **explicit
/// no-tools** grant, `Some(globs)` = **narrow**. The Team tab renders all three
/// distinctly rather than showing the full company allow-list for a teammate the
/// operator emptied.
///
/// Its callers have already established that `agent_id` is on the roster, so a
/// miss in the manifest half can only be the overlay half; a genuine miss reads
/// as `None`, which the callers treat as the inherit default.
pub(super) fn requested_grants(record: &CompanyRecord, agent_id: &str) -> Option<Vec<String>> {
    if let Some(agent) = record.effective_agent(agent_id) {
        return agent.tools.clone();
    }
    record
        .overlay_agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.tools.clone())
}

/// The **declared** cognition-tier hint for `agent_id`: the manifest
/// `[[agent]].tier` line verbatim, or `None` when the row declares none — and
/// for every overlay teammate, which has no manifest row to declare one.
///
/// Verbatim is the whole contract. This is what the company *wrote*, not a
/// resolved answer, and `None` means **undeclared** — a reader has to render
/// that as "cannot say" rather than substituting a default. Issue #643 is
/// exactly that substitution: the overview graph printed a literal `worker` for
/// every teammate, so a company declaring `tier = "orchestrator"` read back as
/// a worker on its own graph.
///
/// Sibling of [`requested_grants`] in shape and in reason: one lookup, shared
/// by the roster list and the detail read, so the two cannot answer differently
/// for the same teammate.
pub(super) fn declared_tier(record: &CompanyRecord, agent_id: &str) -> Option<String> {
    record
        .manifest
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.tier.clone())
}

/// The declared per-agent model override for `agent_id`, from whichever half
/// of the roster it comes from (issue #1245's per-agent follow-up).
///
/// Unlike [`declared_tier`], this checks **both** the manifest row and the
/// overlay row — `Agent::model` and `OverlayAgent::model` are siblings that
/// both exist, since a model override (unlike a tier tag) is something an
/// operator-defined teammate can carry too. `None` means undeclared, exactly
/// as `declared_tier`'s own contract: this is what the roster *wrote*, not a
/// resolved answer.
pub(super) fn declared_model(record: &CompanyRecord, agent_id: &str) -> Option<String> {
    // Through `effective_agent`, not `manifest.agents` directly: a blueprint
    // teammate's edit is stored as an overlay, and reading the raw manifest
    // row skips it. That is what made a `PATCH` here look successful and read
    // back at the old value — the write landed, this never looked at it.
    record
        .effective_agent(agent_id)
        .and_then(|agent| agent.model.clone())
        .or_else(|| {
            record
                .overlay_agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .and_then(|agent| agent.model.clone())
        })
}

/// The declared provider half of the pair for `agent_id` (keys rework, issue
/// #2306, slice 3a) — a sibling of [`declared_model`] in every respect,
/// including the same reason it reads through `effective_agent` rather than
/// the raw manifest row: a blueprint teammate's edit is stored as an overlay.
pub(super) fn declared_provider(record: &CompanyRecord, agent_id: &str) -> Option<String> {
    record
        .effective_agent(agent_id)
        .and_then(|agent| agent.provider.clone())
        .or_else(|| {
            record
                .overlay_agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .and_then(|agent| agent.provider.clone())
        })
}

/// The declared harness binding for `agent_id`, from whichever half of the
/// roster it comes from (issue #1245's harness-picker follow-up).
///
/// Sibling of [`declared_model`] in shape and in reason: `Agent::harness` and
/// `OverlayAgent::harness` are the same field on both roster halves now, and
/// `None` means "the default harness", not "undeclared" — unlike
/// [`declared_tier`], every teammate resolves to *some* harness, this just
/// says whether it named one explicitly.
pub(super) fn declared_harness(record: &CompanyRecord, agent_id: &str) -> Option<String> {
    // Through `effective_agent`, not `manifest.agents` directly: a blueprint
    // teammate's edit is stored as an overlay, and reading the raw manifest
    // row skips it. That is what made a `PATCH` here look successful and read
    // back at the old value — the write landed, this never looked at it.
    record
        .effective_agent(agent_id)
        .and_then(|agent| agent.harness.clone())
        .or_else(|| {
            record
                .overlay_agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .and_then(|agent| agent.harness.clone())
        })
}

/// Whether `agent_id` is this company's orchestrator.
///
/// Delegates to [`crate::company::orchestrator_id`] — the roster rule the
/// harness itself resolves the orchestrator with (the agent tagged with the
/// orchestrator tier, else the first declared agent), never a re-read of
/// [`declared_tier`].
///
/// **This is not the same question as the tier.** A company that tags nobody
/// still has an orchestrator, so an untagged first agent answers `true` here
/// while [`declared_tier`] answers `None`; and a *second* agent tagged with the
/// orchestrator tier carries that tier while answering `false` here, because the
/// rule picks one. A caller that re-derived the marker from the tier string
/// would get both of those backwards.
pub(super) fn is_orchestrator(record: &CompanyRecord, agent_id: &str) -> bool {
    crate::company::orchestrator_id(&record.effective_agents()) == Some(agent_id)
}

/// Whether `agent_id` came from the **global baseline**
/// ([`crate::globals`]) rather than from this company.
///
/// Provenance, and the one question first-run setup turns on (issue #1404).
/// `apply_globals` appends `companies/_globals/agents/*.toml` to *every* company's roster
/// whatever its manifest says, so "is the roster empty?" is answered `no` on a
/// company nobody has ever staffed — which is how the whole first-run flow came
/// to be unreachable in the shipped product. The console needs to tell the
/// baseline apart from a team, and it must not do that by hard-coding the
/// baseline's ids: the next global added would silently re-break the gate.
///
/// Read from [`Agent::global`](crate::company::Agent::global), the marker the
/// merge itself sets, so this answer moves with the baseline rather than
/// alongside it. An overlay teammate is never global — the merge only ever
/// touches the manifest roster — so an id this does not find is `false`, which
/// is also the right answer for an id that is not on the roster at all.
pub(super) fn is_global(record: &CompanyRecord, agent_id: &str) -> bool {
    record
        .manifest
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .is_some_and(|agent| agent.global)
}

/// One agent's grants at all three levels — the single constructor for
/// [`AgentToolsDto`].
///
/// `effective` comes from
/// [`agent_effective_grants`](crate::runtime::builder::agent_effective_grants),
/// the same function the harness builds the agent with, for the reason the
/// module docs give. This function exists so the **roster list** and the
/// **detail read** cannot answer that question differently either (issue
/// #601): the overview graph reads the list and used to invent a tool shelf by
/// dealing slices of `[tools].allow`, so the graph and the detail card beside
/// it disagreed about the same agent. Sharing the constructor makes that
/// disagreement unrepresentable rather than merely fixed once.
/// Takes the `record` and `agent_id` rather than a pre-extracted allow-list,
/// because the desk level cannot be derived from the company grant alone — it
/// depends on which desks this teammate sits on. Passing the record is what makes
/// "forgot to apply the desk ceiling" unrepresentable at the call site rather
/// than a thing three callers each have to remember.
pub(super) fn agent_tools(record: &CompanyRecord, agent_id: &str) -> AgentToolsDto {
    let company_allow = &record.manifest.tools.allow;
    let requested = requested_grants(record, agent_id);

    // The desk ceilings this agent is under, resolved through the record's
    // *effective* desk membership so a console-seated member is scoped exactly
    // as a manifest one.
    let desk_tools = record.agent_desk_tools(agent_id);
    let desk_refs: Vec<&[String]> = desk_tools.iter().map(Vec::as_slice).collect();

    // Reported already narrowed by the company grant, so the console can render
    // the three rows as a strictly shrinking chain. A raw union could show a
    // desk "granting" something the company never allowed.
    //
    // `desk_ceiling_active` is a separate flag rather than `!desk_allow.is_empty()`:
    // the narrowed list can resolve to empty while a ceiling is still in play
    // (a desk whose only grant the company's `*` does not confer), and the
    // console has to keep the desk level as the gate in that case instead of
    // falling back to the company allow-list.
    let desk_ceiling_active = !desk_tools.iter().all(Vec::is_empty);
    let desk_allow = if desk_ceiling_active {
        // The desk ceiling as it stands with the agent contributing nothing —
        // `None` (inherit), not `Some(&[])` (deny-all): this row previews what
        // the desks grant a teammate that has stated no scope of its own.
        agent_scoped_grants(company_allow, &desk_refs, None)
    } else {
        Vec::new()
    };

    AgentToolsDto {
        effective: agent_scoped_grants(company_allow, &desk_refs, requested.as_deref()),
        requested,
        company_allow: company_allow.to_vec(),
        desk_allow,
        desk_ceiling_active,
    }
}

/// The `PATCH` body. Every field is optional, and an absent field is left
/// alone: this is a patch, not a replacement, so a console that renders only
/// some of an agent's fields cannot blank the rest by omission.
///
/// `description` is a **double option** so "leave it" and "clear it" stay
/// apart on the wire, the same shape and for the same reason as
/// [`SetBudget`](super::team::SetBudget)'s cap:
///
/// | body | parses as | means |
/// |---|---|---|
/// | `{}` | `None` | leave the description alone |
/// | `{"description": null}` | `Some(None)` | clear it |
/// | `{"description": "…"}` | `Some(Some(…))` | set it |
///
/// Collapsing the first two would make every partial save silently erase an
/// agent's instructions, which is the single worst thing this route could do.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditAgent {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    description: Option<Option<String>>,
    /// The teammate's tool scope (issues #619, #1804). A **double option**,
    /// because since #1804 the grant has three representable states and "leave it
    /// alone" has to stay apart from every one of them:
    ///
    /// | body | parses as | means |
    /// |---|---|---|
    /// | `{}` | `None` | leave the scope alone |
    /// | `{"tools": null}` | `Some(None)` | reset to the company's **standard grant** (inherit) |
    /// | `{"tools": []}` | `Some(Some([]))` | an **explicit no-tools** grant (deny-all) |
    /// | `{"tools": ["…"]}` | `Some(Some([…]))` | **narrow** to those globs |
    ///
    /// This is the deliberate contract inversion #1804 makes: before it, `[]`
    /// was documented as "reset to standard". `[]` now means deny-all and the
    /// reset moves to `null`. The failure mode of an out-of-date client sending
    /// the old `[]` is soft — it removes capability rather than granting it.
    ///
    /// #661 made a teammate scopable at *creation* (`POST …/team` and
    /// `add_agent`). This is the half that was missing: re-scoping one that
    /// already exists, without deleting and recreating it — which would orphan
    /// its workspace folder, budget row, desk memberships and inbox.
    #[serde(default, deserialize_with = "double_option")]
    tools: Option<Option<Vec<String>>>,
    /// The teammate's persona instructions (issue #1530). A **double option**,
    /// the same three-state contract as `description`:
    ///
    /// | body | parses as | means |
    /// |---|---|---|
    /// | `{}` | `None` | leave the instructions alone |
    /// | `{"instructions": null}` | `Some(None)` | clear the override → reset to blueprint |
    /// | `{"instructions": "…"}` | `Some(Some(…))` | set the override |
    ///
    /// Unlike every other field here, this is accepted for a **manifest**
    /// teammate too: it writes to the per-agent override record, not to
    /// `company.toml`, so it is legal for both kinds. A blank/whitespace string
    /// is normalized to a reset, so an override can never blank a persona.
    #[serde(default, deserialize_with = "double_option")]
    instructions: Option<Option<String>>,
    /// The face this teammate wears (`docs/spec/runtime/avatars.md`). A
    /// **double option**, the same three-state contract as `instructions`:
    ///
    /// | body | parses as | means |
    /// |---|---|---|
    /// | `{}` | `None` | leave the face alone |
    /// | `{"avatar": null}` | `Some(None)` | reset to the mascot hashed from the id |
    /// | `{"avatar": "tiny:teal"}` | `Some(Some(…))` | wear that face |
    ///
    /// Accepted for a **manifest** teammate as well, for the reason
    /// `instructions` is: it writes to the per-agent override record rather than
    /// to `company.toml`. Editable by any member rather than admin-only —
    /// picking a colleague's face is not a privilege boundary the way widening a
    /// tool grant is, and a company whose only admin is away should not be stuck
    /// with eleven hashed blobs.
    ///
    /// Validated by [`crate::company::avatar::normalize`], so the only strings
    /// that reach the record name something this host already holds.
    #[serde(default, deserialize_with = "double_option")]
    avatar: Option<Option<String>>,
    /// Whether this teammate's `mascot:animated` canvas plays. Same
    /// double-option contract as `avatar`:
    ///
    /// | body | parses as | means |
    /// |---|---|---|
    /// | `{}` | `None` | leave the mode alone |
    /// | `{"mascotMode": null}` | `Some(None)` | reset to the file's own default mode (`"animated"`) |
    /// | `{"mascotMode": "static"}` | `Some(Some(…))` | wear that mode |
    ///
    /// Meaningful only alongside a `mascot:` `avatar`, but not refused when
    /// sent without one — the same "store the choice, apply it once the right
    /// avatar is worn" latitude a picker UI needs when it lets an operator
    /// preview a mode before committing the mascot itself. Validated by
    /// [`crate::company::mascot::parse_mode`] against the closed list. Open
    /// to any member, matching `avatar`: picking a colleague's display mode
    /// is not a privilege boundary.
    #[serde(default, deserialize_with = "double_option")]
    mascot_mode: Option<Option<String>>,
    /// The mascot costume this teammate wears. Same double-option contract
    /// and member-open gate as `mascot_mode`; `null` resets to the file's own
    /// default costume, an id sets it. Applies whichever mode is in force.
    /// Validated by [`crate::company::mascot::parse_costume`].
    #[serde(default, deserialize_with = "double_option")]
    mascot_costume: Option<Option<String>>,
    /// The mascot's skin (body) color. Same double-option contract and
    /// member-open gate as `mascot_mode`. Validated by
    /// [`crate::company::mascot::parse_skin_color`].
    #[serde(default, deserialize_with = "double_option")]
    mascot_skin_color: Option<Option<String>>,
    /// The mascot's hand/accent color. Same double-option contract and
    /// member-open gate as `mascot_mode`. Validated by
    /// [`crate::company::mascot::parse_hand_color`].
    #[serde(default, deserialize_with = "double_option")]
    mascot_hand_color: Option<Option<String>>,
    /// The teammate's own model override (issue #1245's per-agent follow-up).
    /// A double option for the same reason as `description`: absent leaves it
    /// alone, `null` clears it back to the harness's own default, and a
    /// string sets it. Admin-only, alongside `tools` — see [`edit_agent`]:
    /// a model choice carries the same "this is a cost/scope decision, not a
    /// teammate's own detail" character `tools` does, not a name or a role.
    #[serde(default, deserialize_with = "double_option")]
    model: Option<Option<String>>,
    /// Which declared `[[harness]]` this teammate runs on (issue #1245's
    /// harness-picker follow-up). Same double-option shape and the same
    /// admin gate as `model` — see [`edit_agent`]. `null` clears it back to
    /// the harness marked `default = true`; a string pins it to one of the
    /// ids `GET {scope}/harnesses` lists. Validated against that same list at
    /// write time, so a typo or a stale id from a client that cached an old
    /// harness list is a `400`, not a teammate silently orphaned from every
    /// harness's serve set.
    #[serde(default, deserialize_with = "double_option")]
    harness: Option<Option<String>>,
    /// The provider half of this teammate's `{provider, model}` pair (keys
    /// rework, issue #2306, slice 3a). Same double-option shape and the same
    /// admin gate as `model` — see [`edit_agent`]: `null` clears it (back to
    /// the company default), a slug sets it. Sent together with `model` by
    /// the console; the host validates the pair it is about to store rather
    /// than one half against the other.
    #[serde(default, deserialize_with = "double_option")]
    provider: Option<Option<String>>,
}

/// `GET {scope}/team/{agent_id}` — one agent, read.
async fn agent_detail(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Path(AgentPath { agent_id }): Path<AgentPath>,
) -> Result<Json<AgentDetailDto>, ApiError> {
    // Only to decide what `editable` may claim — the read itself is open to any
    // member, unchanged. A principal this cannot resolve reads as not-admin,
    // which is fail-closed in the right direction: it under-claims what the
    // caller may edit rather than over-claiming it.
    let is_admin = is_admin_actor(&headers, &state, &company, peer).await;
    let record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;
    detail(&company, &record, &agent_id, is_admin).await
}

/// `PATCH {scope}/team/{agent_id}` — edit a teammate.
///
/// Refuses an unknown id with a `404`. A manifest teammate's version-controlled
/// fields (`name`/`role`/`description`/`tools`) still refuse with a `409` naming
/// where the edit belongs — but since #1530 the refusal is **conditional**: an
/// `instructions`-only body is accepted for a manifest teammate too, because it
/// writes to the per-agent override record rather than `company.toml` (see the
/// check inside). `name`, `role`, `description` and `instructions` are open to
/// any signed-in member, matching `POST …/team`: defining a teammate was never
/// admin-only, so correcting one it defined is not either.
///
/// # Why three fields are the exception (issues #619, #1245)
///
/// That reasoning covers what a teammate *is*. It does not cover what a
/// teammate may *do* or *run on*, and the three admin-gated fields are the
/// second thing — the [`AdminScopedCompany`](super::AdminScopedCompany) axis: a
/// write that settles something *on behalf of* the company rather than one a
/// member makes for themselves.
///
/// `tools` is the sharpest edge. Since #1804 it is three-state: **`null` means
/// "reset to the company's standard grant"** — the widest grant the company
/// has — while `[]` is a deliberate deny-all and `[globs]` narrows. So
/// `{"tools": null}` is not a small edit, it is a *widening*, and left
/// member-open it would let any signed-in member hand a deliberately-scoped
/// teammate the company's whole grant back. That is the exact inversion this
/// field was added to prevent. Every `tools` state is admin-gated regardless,
/// so a member cannot narrow to a deny-all either.
///
/// `model` and `harness` are admin-gated for the same *kind* of reason without
/// that sharp edge: both are routing decisions the company owns rather than
/// details of the teammate. A model override names the inference this company
/// is paying for; a harness binding pins which serve set the teammate runs on.
/// Neither is a name or a role the account holder would edit for themselves, so
/// both sit with the grant on the admin side of the line.
///
/// So the admin check is **conditional on the fields being present**, in the
/// same shape and for the same reason as the cap on
/// [`add_member`](super::team): a member who edits a name or a role keeps
/// working exactly as before, and adding these fields must not quietly take an
/// existing capability away from members.
///
/// Being conditional is also what fixes its **position**: it runs after the
/// `409`/`404` checks, so an unknown id answers `404` whether or not the body
/// carried an admin-gated field. See the comment at the check itself.
///
/// Narrow-only-for-members was considered and rejected: it makes the scope a
/// one-way ratchet, so a teammate scoped too tightly could never be loosened by
/// anyone, and the only way back would be delete-and-recreate — which orphans
/// the workspace folder, budget row, desk memberships and inbox this route
/// exists to preserve.
async fn edit_agent(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Path(AgentPath { agent_id }): Path<AgentPath>,
    Json(body): Json<EditAgent>,
) -> Result<Json<AgentDetailDto>, crate::server::Rejection> {
    // Identity before validation, and before the avatar below is resolved.
    //
    // A `blob:` avatar streams up to 4 MiB from the workspace backend, and that
    // resolution is deliberately moved ahead of the write lock (see the note
    // there). That ordering must not also move it ahead of the roster check: an
    // id that names nobody has a `404` coming, not a `400` (or up to 4 MiB of
    // I/O) spent proving the shape of a body nobody could have applied. So when
    // the body carries an avatar, the roster is read once, unlocked, and an
    // unknown id is refused before any avatar work; the lock below re-reads and
    // re-checks, because the roster may have changed while the avatar was
    // resolving. A body without an avatar has nothing slow to get ahead of, so
    // the single locked check below is enough for it.
    if body.avatar.is_some() {
        let early = company
            .runtime
            .store()
            .load(company.id())
            .await?
            .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;
        if !early.is_roster_agent(&agent_id) {
            return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
                "teammate {agent_id}"
            )))
            .into_response()
            .into());
        }
    }

    // A submitted face is resolved *before* the write lock below is taken.
    //
    // A `blob:` avatar streams up to 4 MiB from the workspace backend, and the
    // bytes it resolves to do not depend on the record — so holding the
    // per-company write lock across that I/O would let a slow or stalled remote
    // store block every other roster and policy write, on a request any member
    // can repeat. The immutable reference is resolved here instead, and the
    // lock below is held only for the load-mutate-save of the record.
    //
    // `None` is "field absent" (no change), `Some(None)` is "clear it back to
    // the hashed default", `Some(Some(ref))` is the stored reference.
    let resolved_avatar: Option<Option<String>> = match &body.avatar {
        None => None,
        Some(avatar) => {
            let value = avatar.as_deref().map(str::trim).filter(|v| !v.is_empty());
            match value {
                Some(value) => {
                    let stored = crate::company::avatar::resolve(
                        company.runtime.workspace().as_ref(),
                        company.id(),
                        value,
                    )
                    .await
                    .map_err(|e| ApiError(e).into_response())?;
                    Some(Some(stored))
                }
                None => Some(None),
            }
        }
    };

    // The mascot mode/costume/colors need no I/O to validate — all four are
    // closed, in-memory lists — so unlike `avatar` they are checked here
    // rather than resolved, and the checked values are what gets written
    // under the lock below. Same double-option unwrap shape as
    // `resolved_avatar`.
    let resolved_mascot_mode: Option<Option<String>> = match &body.mascot_mode {
        None => None,
        Some(None) => Some(None),
        Some(Some(value)) => {
            let parsed = crate::company::mascot::parse_mode(value)
                .map_err(|e| ApiError(e).into_response())?;
            Some(Some(parsed.to_string()))
        }
    };
    let resolved_mascot_costume: Option<Option<String>> = match &body.mascot_costume {
        None => None,
        Some(None) => Some(None),
        Some(Some(value)) => {
            let parsed = crate::company::mascot::parse_costume(value)
                .map_err(|e| ApiError(e).into_response())?;
            Some(Some(parsed.to_string()))
        }
    };
    let resolved_mascot_skin_color: Option<Option<String>> = match &body.mascot_skin_color {
        None => None,
        Some(None) => Some(None),
        Some(Some(value)) => {
            let parsed = crate::company::mascot::parse_skin_color(value)
                .map_err(|e| ApiError(e).into_response())?;
            Some(Some(parsed.to_string()))
        }
    };
    let resolved_mascot_hand_color: Option<Option<String>> = match &body.mascot_hand_color {
        None => None,
        Some(None) => Some(None),
        Some(Some(value)) => {
            let parsed = crate::company::mascot::parse_hand_color(value)
                .map_err(|e| ApiError(e).into_response())?;
            Some(Some(parsed.to_string()))
        }
    };

    // Serialize with every other write to `overlay_agents`, so a console edit
    // and a concurrent `add_agent` cannot clobber one another's roster.
    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;

    // The roster was already checked, unlocked, above — but the write lock was
    // taken and the record re-loaded *after* the avatar resolved, and a
    // concurrent add or retirement can have changed the roster in between. So
    // the id is re-checked against the locked load before anything is mutated.
    //
    // Identity before validation, so an unknown id is a 404 rather than a
    // complaint about the shape of a body nobody could have applied anyway.
    //
    // A **manifest** teammate is edited through the override layer below rather
    // than refused: a company you have deployed is still yours to change, and
    // the blueprint is never rewritten either way.
    // Asked through `is_roster_agent`, which is the same union `detail` reads
    // back through — and, crucially, excludes a teammate the operator has
    // removed. A retired manifest id still matches `manifest.agents`, so a
    // narrower check here would store an override for a teammate that is not on
    // the roster and then answer `404` from `detail`: a failed request that
    // mutated the record on its way out.
    if !record.is_roster_agent(&agent_id) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "teammate {agent_id}"
        )))
        .into_response()
        .into());
    }
    let is_manifest = record.manifest.agents.iter().any(|a| a.id == agent_id);

    // Authority **after** existence, and this ordering is forced rather than
    // preferred (review of #745).
    //
    // The check is conditional on the admin-gated fields, so putting it first
    // would make one route give two answers about whether a teammate exists:
    // `{"name": "x"}` on an unknown id would 404 while `{"tools": […]}` on the
    // same id would 403. Nothing about an unrelated field should decide that,
    // and the member-open path cannot be moved to match — a name edit is
    // member-open and has no authority check to run first. So this is the only
    // order in which the two paths agree.
    //
    // The usual reason to authorise first — refusing to confirm a resource
    // exists — does not apply: `GET {scope}/team/{agent_id}` is open to any
    // signed-in member and already 404s on an unknown id, so ordering 403
    // ahead of 404 here would hide nothing from the very caller it would
    // inconvenience.
    //
    // Deliberately unlike `set_budget`, which authorises first: that route is
    // admin-only in full, so admin-first is self-consistent there. This one is
    // admin-only *per field*, which is what makes the ordering load-bearing.
    if body.tools.is_some()
        || body.model.is_some()
        || body.harness.is_some()
        || body.provider.is_some()
    {
        require_admin(&headers, &state, &company.runtime, peer).await?;
    }

    let name = trimmed_field(body.name.as_deref(), "name").map_err(|e| e.into_response())?;
    let role = trimmed_field(body.role.as_deref(), "role").map_err(|e| e.into_response())?;
    // The double option is preserved end to end: the outer layer says whether
    // the field was sent at all (leave-alone vs set), the inner says which of
    // the three grant states it was set to (`None` = reset to standard,
    // `Some([])` = deny-all, `Some(globs)` = narrow). Only the innermost glob
    // list is trimmed.
    let tools: Option<Option<Vec<String>>> = body
        .tools
        .map(|maybe_globs| maybe_globs.map(|globs| trimmed_globs(&globs)).transpose())
        .transpose()
        .map_err(|e| e.into_response())?;
    // Present-and-null clears; a blank string clears too — an empty override
    // and no override mean the same thing (the harness's own default model
    // applies), and storing `Some("")` would only make the two look
    // different on the wire. Hoisted above the mutation below (unlike
    // `tools`/`name`) because the cross-field check just below needs the
    // *resulting* value, not `body.model` itself.
    let model = body
        .model
        .map(|text| text.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    // Same double-option contract, and same reason to hoist: validated below
    // against the declared harness list before anything is written.
    let harness = body
        .harness
        .map(|text| text.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    // The provider half of the pair (keys rework, issue #2306, slice 3a) —
    // same hoist, same blank-means-cleared contract as `model`/`harness`.
    let provider = body
        .provider
        .map(|text| text.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));

    // A coding CLI this build drives is bindable without any `[[harness]]`
    // naming it, and `GET {scope}/harnesses` offers exactly those ids in the
    // picker. But `harness_by_id` resolves an `ACP_AGENTS` id on *any* build
    // via the implicit-local fallback, which would let a hosted admin bind a
    // teammate to a CLI the server has nothing to launch — accepted by `PATCH`,
    // then dead on the next rebuild. So gate that fallback the same way the
    // picker does: declared harnesses (and the built-in when a manifest
    // declares none) are always bindable, an undeclared coding CLI only when
    // this host wires an `AcpAgentFactory`, and anything else is refused.
    if let Some(Some(id)) = &harness {
        let declared = record
            .manifest
            .effective_harnesses()
            .iter()
            .any(|h| h.id == *id);
        // `can_run_local_acp()` rather than `acp_agents().is_some()` — see
        // issue #1814 and the method's own doc. The picker above uses the same
        // predicate, which is the point of it being one method.
        let bindable = declared || (ACP_AGENTS.contains(&id.as_str()) && state.can_run_local_acp());
        if !bindable {
            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                "no harness named `{id}` is available for this company."
            )))
            .into());
        }
    }

    // `model` means one of two unrelated things depending on the harness this
    // edit leaves the teammate on — resolved against the *resulting* binding
    // (the new one when this request sends one, else the current one), so
    // setting a model in the same request as switching harness kind is
    // validated against where the teammate is actually headed, not its stale
    // binding. On `acp` it is the pre-existing per-agent model hint (issue
    // #1245); `CompanyManifest::validate` enforces the identical rule for a
    // manifest agent's own `model`, but an overlay teammate never passes
    // through that validation (it lives on the record, not the parsed
    // manifest), so it is repeated here. On `built_in` it is the model half
    // of the `{provider, model}` pair (keys rework, issue #2306, slice 3a),
    // and `provider` is validated alongside it — a partial pair, an unknown
    // provider, or a disabled one are all refused before anything is written.
    let resulting_harness_id = harness
        .clone()
        .unwrap_or_else(|| declared_harness(&record, &agent_id))
        .unwrap_or_else(|| record.manifest.default_harness_id());
    let bound = record.manifest.harness_by_id(&resulting_harness_id);
    let on_acp = bound.as_ref().map(|h| h.kind.as_str()) == Some("acp");
    let resulting_model = model
        .clone()
        .unwrap_or_else(|| declared_model(&record, &agent_id));
    let resulting_provider = provider
        .clone()
        .unwrap_or_else(|| declared_provider(&record, &agent_id));

    // Keys rework (#2306), round-3a review P2-2: held from the provider
    // check below through the record save (`record.upsert_agent_override` /
    // `agent.provider = …` and `store().save(&record)`), so a concurrent
    // provider delete, disable or key clear cannot pass its own `usedBy`
    // check against a pair this request is about to write — every provider
    // mutation that could strand this pair now takes the same lock.
    // `write_lock` (the roster lock, taken above) always outranks it here:
    // this is the only site that holds both, and it always acquires
    // `write_lock` first — see `index_lock`'s own lock-order note in
    // `company/inference/store.rs`. Never held across a network call: the
    // check below is a secret-store read, and the ACP branch above returns
    // before ever reaching here.
    let _index_guard = crate::company::inference::store::index_lock(company.id()).await;

    if on_acp {
        // An ACP agent brings its own credential — a provider naming a
        // console-managed one has nowhere to go, independent of `model`.
        if let Some(provider_value) = &resulting_provider {
            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                "`{provider_value}` names a provider, but harness `{resulting_harness_id}` is \
                 an ACP harness, which brings its own. Clear the provider, or bind a built-in \
                 harness."
            )))
            .into());
        }
        if let Some(model_value) = &resulting_model {
            // `kind = "acp"` is not sufficient: a `runner` transport is ACP
            // and still cannot carry a model, because the runner wire
            // protocol has no field for one. `CompanyManifest::validate`
            // already refuses this combination, so accepting it here let the
            // API store a binding a manifest is not allowed to declare — and
            // one that could never take effect. The wording is the
            // validator's, so both refusals read the same.
            if bound
                .as_ref()
                .and_then(|h| h.acp.as_ref())
                .map(|acp| acp.transport.as_str())
                == Some("runner")
            {
                return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                    "`{model_value}` names a model, but harness `{resulting_harness_id}` uses \
                     `transport = \"runner\"`. Model overrides aren't supported for a runner \
                     yet — the runner wire protocol doesn't carry them."
                )))
                .into());
            }
        }
    } else {
        match (&resulting_provider, &resulting_model) {
            (None, None) => {}
            (Some(provider_value), None) => {
                return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                    "Choose a model for `{provider_value}`, or clear the provider to use the \
                     company default."
                )))
                .into());
            }
            (None, Some(model_value)) => {
                return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                    "`{model_value}` names a model but no provider. Choose a provider too, or \
                     clear the model to use the company default."
                )))
                .into());
            }
            (Some(provider_value), Some(model_value)) => {
                // Only when this request actually touches the pair or the
                // harness binding: a name-only edit must not 400 because a
                // provider was switched off since the pair was saved — see
                // the gotcha this reasoning shares with `resolve_for_turn`'s
                // own fail-closed check at turn time, which is where a pin
                // that goes bad *after* being saved is caught instead.
                if provider.is_some() || model.is_some() || harness.is_some() {
                    match crate::company::inference::store::get_provider(
                        company.id(),
                        company.runtime.secrets().as_ref(),
                        provider_value,
                    )
                    .await
                    .map_err(ApiError)?
                    {
                        None => {
                            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                                "This company has no provider `{provider_value}`. Add it in \
                                 Connections → API Keys → LLM first."
                            )))
                            .into());
                        }
                        Some(row) if !row.enabled => {
                            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                                "Provider `{}` is switched off. Switch it on in Connections → \
                                 API Keys → LLM, or choose another.",
                                row.label
                            )))
                            .into());
                        }
                        Some(_) => {}
                    }
                    crate::company::inference::store::check_model_id(model_value)
                        .map_err(|why| ApiError(why).into_response())?;
                }
            }
        }
    }

    // Captured before the three are consumed below. `Some` means the request
    // carried the field at all — including a `null` that clears it, which
    // changes routing exactly as much as setting one does.
    let routing_changed = model.is_some() || harness.is_some() || provider.is_some();

    if is_manifest {
        // Stored as an overlay on the record, exactly like the daily-budget
        // override #343 modelled: `company.toml` keeps saying what the company
        // launched with, and this says what the operator has since decided. The
        // merge is field-wise, so a field nobody edited keeps tracking the
        // blueprint across a redeploy.
        let mut entry = crate::ports::types::AgentOverride {
            agent_id: agent_id.clone(),
            name,
            role,
            tools,
            ..Default::default()
        };
        // An empty string is the stored form of "cleared" — the write path
        // already treats a blank description and no description as one state.
        if let Some(description) = body.description {
            entry.description = Some(
                description
                    .map(|text| text.trim().to_string())
                    .unwrap_or_default(),
            );
        }
        // Issue #1245's per-agent follow-up. These were advertised as editable
        // and accepted by this handler, but the override built here carried
        // only the four fields above — so a blueprint teammate's harness or
        // model edit returned 200 and was then read back at its old value,
        // with nothing anywhere reporting the loss. Blank is the stored form
        // of "cleared", exactly as for `description`.
        if let Some(model) = model {
            entry.model = Some(model.unwrap_or_default());
        }
        if let Some(harness) = harness {
            entry.harness = Some(harness.unwrap_or_default());
        }
        // The provider half of the pair (keys rework, issue #2306, slice 3a):
        // same blank-means-cleared contract, same reason to carry it —
        // cross-validated above alongside `model`.
        if let Some(provider) = provider {
            entry.provider = Some(provider.unwrap_or_default());
        }
        record.upsert_agent_override(entry);
    } else {
        let agent = record
            .overlay_agents
            .iter_mut()
            .find(|a| a.id == agent_id)
            .expect("overlay membership was checked above");
        if let Some(name) = name {
            agent.name = name;
        }
        if let Some(role) = role {
            agent.role = role;
        }
        // Present-and-null clears; a blank string clears too, since an empty
        // description and no description frame the persona identically and
        // storing `Some("")` would only make the two look different on the wire.
        if let Some(description) = body.description {
            agent.description = description
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
        }
        // Issues #619, #1804: stored verbatim, exactly like a manifest
        // `[[agent]].tools` line, in its three-state form — `None` (inherit the
        // standard grant), `Some([])` (explicit deny-all), or `Some(globs)`
        // (narrow). The outer option here is "was the field sent"; the inner is
        // the grant state, which is exactly `OverlayAgent::tools`. The company
        // `allow` ceiling is applied at *read* time by `agent_effective_grants`,
        // so a glob the company does not cover is surfaced as
        // asked-for-but-not-granted rather than silently dropped here — and this
        // route can only ever narrow a teammate within a grant the company made.
        if let Some(tools) = tools {
            agent.tools = tools;
        }
        // Issue #1245's per-agent follow-up: already trimmed/blank-cleared
        // and cross-validated above.
        if let Some(model) = model {
            agent.model = model;
        }
        if let Some(harness) = harness {
            agent.harness = harness;
        }
        // Keys rework, issue #2306, slice 3a: already trimmed/blank-cleared
        // and cross-validated above.
        if let Some(provider) = provider {
            agent.provider = provider;
        }
    }

    // Issue #1530: the persona override, written to the record for **either**
    // kind. A `null` or blank/whitespace body normalizes to a reset — drop the
    // override so the blueprint `prompt` applies again — which is what keeps an
    // emptied edit from silently blanking a persona. A non-empty string upserts,
    // replacing any prior override so `agent_override`'s first-match read can
    // never see a stale row.
    if let Some(instructions) = body.instructions {
        match instructions
            .map(|text| crate::company::prompt::cap_persona_instructions(text.trim()))
            .filter(|text| !text.is_empty())
        {
            Some(text) => record.upsert_agent_override(AgentOverride {
                agent_id: agent_id.clone(),
                instructions: Some(text),
                ..Default::default()
            }),
            None => record.clear_agent_override(&agent_id),
        }
    }

    // The chosen face, written to the same override row for either kind of
    // teammate. `null` — and a blank string, which is the same intent typed by a
    // client that cleared an input — resets to the hashed default rather than
    // storing an unrenderable empty reference. The bytes were resolved before
    // the write lock above (see the note at the top of this handler), so this
    // only writes the outcome under the lock.
    if let Some(avatar) = resolved_avatar {
        match avatar {
            Some(stored) => record.upsert_agent_override(AgentOverride {
                agent_id: agent_id.clone(),
                avatar: Some(stored),
                ..Default::default()
            }),
            None => record.clear_agent_avatar(&agent_id),
        }
    }

    // The chosen mascot mode/costume/colors, written the same field-wise way
    // as `avatar` — each validated above, with no I/O to get ahead of the
    // write lock for.
    if let Some(mode) = resolved_mascot_mode {
        match mode {
            Some(value) => record.upsert_agent_override(AgentOverride {
                agent_id: agent_id.clone(),
                mascot_mode: Some(value),
                ..Default::default()
            }),
            None => record.clear_agent_mascot_mode(&agent_id),
        }
    }
    if let Some(costume) = resolved_mascot_costume {
        match costume {
            Some(value) => record.upsert_agent_override(AgentOverride {
                agent_id: agent_id.clone(),
                mascot_costume: Some(value),
                ..Default::default()
            }),
            None => record.clear_agent_mascot_costume(&agent_id),
        }
    }
    if let Some(skin_color) = resolved_mascot_skin_color {
        match skin_color {
            Some(value) => record.upsert_agent_override(AgentOverride {
                agent_id: agent_id.clone(),
                mascot_skin_color: Some(value),
                ..Default::default()
            }),
            None => record.clear_agent_mascot_skin_color(&agent_id),
        }
    }
    if let Some(hand_color) = resolved_mascot_hand_color {
        match hand_color {
            Some(value) => record.upsert_agent_override(AgentOverride {
                agent_id: agent_id.clone(),
                mascot_hand_color: Some(value),
                ..Default::default()
            }),
            None => record.clear_agent_mascot_hand_color(&agent_id),
        }
    }

    company.runtime.store().save(&record).await?;

    // Release both locks before the possible rebuild below (PR #1875 review
    // finding): `rebuild_company` now serializes its own load-through-save of
    // the record on this same write lock, and this task holding it while
    // calling in would deadlock a non-reentrant `tokio::sync::Mutex` against
    // itself. The save above already landed under both locks; nothing past
    // this point still needs either held. Released in acquisition-reverse
    // order (`index_lock`, taken second, drops first).
    drop(_index_guard);
    drop(_lock);

    // A harness or model change needs the runtime rebuilt, not just saved.
    //
    // Lanes, router bindings and `LocalAcpAgent`'s model map are snapshots
    // `RuntimeBuilder` takes once, and `HarnessBrain::refresh_record` refreshes
    // only the record — so without this the save is durable and inert: it
    // survives, reads back correctly, and changes nothing about where turns go
    // until the process restarts. The same reasoning `inference.rs` applies to
    // a provider change, which is likewise chosen at build time.
    //
    // Only for these two fields. A name, role, tools or description edit does
    // not affect routing, and rebuilding a company for one would be a large
    // cost for no effect.
    // A let-chain rather than a nested `if`: the tuple form clippy's
    // `collapsible_if` suggests would evaluate `rebuild_company` before
    // testing the flag, rebuilding on every name edit — the exact cost this
    // guard exists to avoid.
    if routing_changed
        && let Err(error) = crate::runtime::rebuild_company(&state, company.id()).await
    {
        // Not fatal, and deliberately not a failed response: the edit *is*
        // saved and will apply on the next start. A host that cannot rebuild
        // in place (no rebuilder wired) is an ordinary configuration, not an
        // error the operator caused by editing a teammate.
        tracing::warn!(
            %error,
            agent = %agent_id,
            "saved the harness binding but could not rebuild the company runtime; \
             it applies on the next restart"
        );
    }

    // The caller either passed `require_admin` above or sent no `tools`, so
    // re-resolve rather than assume: an admin editing only a name must still
    // read back `tools` as editable.
    let is_admin = is_admin_actor(&headers, &state, &company, peer).await;
    detail(&company, &record, &agent_id, is_admin)
        .await
        .map_err(|e| e.into_response().into())
}

/// Rejects a field that was sent but is blank, and trims one that was sent.
///
/// A teammate whose name is whitespace renders as an empty card with no way
/// back to it, so the refusal is a `400` rather than a stored blank.
///
/// The error is an [`ApiError`], **not** the `Response` its caller returns.
/// `clippy::result_large_err` fires on the second shape here and is right to:
/// an `axum` `Response` is 128+ bytes, so a `Result<Option<String>, Response>`
/// makes every successful call carry the footprint of the refusal it did not
/// make. The handler is exempt only because its own `Ok` variant is larger
/// still. The caller converts at the boundary, which is also what the sibling
/// refusal helpers in `team.rs` do by returning `Option<Response>`.
fn trimmed_field(value: Option<&str>, field: &str) -> Result<Option<String>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "a teammate's {field} can't be empty."
        ))));
    }
    Ok(Some(trimmed.to_string()))
}

/// Trims a submitted tool-scope list, refusing a blank entry and dropping
/// duplicates (issue #619).
///
/// A blank *string* glob (`""` / `"  "`) is a `400` rather than a stored empty
/// string for a sharper reason than tidiness: `""` matches nothing an operator
/// meant, so it would read as a scope that grants nothing while looking like a
/// scope that was set. Duplicates are dropped rather than refused — a repeated
/// glob is harmless and the resolved grant list is de-duplicated downstream.
///
/// An empty *list* (`[]`) is **not** an error since issue #1804: it is the
/// explicit deny-all grant, and the caller has already distinguished it from an
/// absent field and from `null` (reset to standard) via the double option. Only
/// a blank entry *inside* a list still 400s.
///
/// Same `ApiError`-not-`Response` return shape as [`trimmed_field`], for the
/// reason given there.
fn trimmed_globs(globs: &[String]) -> Result<Vec<String>, ApiError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(globs.len());
    for glob in globs {
        let trimmed = glob.trim();
        if trimmed.is_empty() {
            return Err(ApiError(OpenCompanyError::InvalidRequest(
                "a tool grant can't be a blank string. Omit `tools` to leave the scope as is, \
                 send `null` to reset it to the company's standard grant, or send an empty list \
                 to give this teammate no tools."
                    .to_string(),
            )));
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

/// Whether the signed-in caller may administer this company — the question
/// [`EDITABLE_FIELDS`] keys off, asked without refusing.
///
/// [`require_admin`] is the enforcement path and returns a `Response` on
/// failure, which is right for a write and wrong for a read that must still
/// succeed for a member. This answers the same question through the same
/// `may_administer` predicate, so the two cannot drift.
async fn is_admin_actor(
    headers: &HeaderMap,
    state: &AppState,
    company: &ScopedCompany,
    peer: Option<std::net::SocketAddr>,
) -> bool {
    crate::server::users::routes::current_user(headers, state, company.id(), peer)
        .await
        .is_some_and(|user| user.may_administer())
}

/// Builds one agent's detail from the loaded record, or 404s when the id names
/// nobody on the roster.
async fn detail(
    company: &ScopedCompany,
    record: &CompanyRecord,
    agent_id: &str,
    is_admin: bool,
) -> Result<Json<AgentDetailDto>, ApiError> {
    // The manifest row with the operator's stored edits applied — the same
    // resolution `build_roster` performs, so the card and the running teammate
    // cannot disagree about who this is.
    let manifest_agent = record.effective_agent(agent_id);
    let overlay_agent = record.overlay_agents.iter().find(|a| a.id == agent_id);

    let (source, name, role, description) = match (manifest_agent.as_deref(), overlay_agent) {
        // A manifest agent wins an id collision, exactly as `build_roster`
        // resolves one: the version-controlled roster is authoritative.
        (Some(agent), _) => (
            AgentSource::Manifest,
            // `None` unless an operator has named this teammate: a manifest
            // `[[agent]]` is addressed by its role, and the console falls back
            // to it when there is no name.
            agent.name.clone(),
            agent.role.clone(),
            agent.description.clone(),
        ),
        // An overlay teammate has no manifest row, so `declared_tier` below
        // misses — and so does `requested_grants`: it holds the company's
        // standard grant, mirroring `harness::overlay_agent_to_manifest`.
        (None, Some(agent)) => (
            AgentSource::Overlay,
            Some(agent.name.clone()),
            agent.role.clone(),
            agent.description.clone(),
        ),
        (None, None) => {
            return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
                "teammate {agent_id}"
            ))));
        }
    };

    let cap = record.effective_budget(agent_id);
    let attribution = record.budget_override(agent_id);
    let spend_today = daily_spend_samples(company, Some(record)).await?;
    let spent = cap.and(
        spend_today
            .as_ref()
            .map(|samples| crate::metering::usd_spent_by_agent(samples, agent_id)),
    );

    let inbox_enabled = company
        .runtime
        .inbox()
        .inboxes(company.id())
        .await?
        .into_iter()
        .any(|meta| meta.key == agent_id && meta.enabled);

    // Issue #1530: the persona in force, the blueprint it would reset to, and
    // whether an override is currently masking that blueprint. `blueprint` is
    // the manifest `prompt` seed — absent for an overlay teammate, which has no
    // manifest row — so the console can preview what "Reset to blueprint"
    // restores. `overridden` is gated on an override that actually carries
    // instructions, so an empty record never reads as "overridden".
    let effective_instructions = record.effective_instructions(agent_id);
    let blueprint_instructions = record
        .manifest
        .agents
        .iter()
        .find(|a| a.id == agent_id)
        .and_then(|a| a.prompt.clone());
    let instructions_overridden = record
        .agent_override(agent_id)
        .is_some_and(|entry| entry.instructions.is_some());

    Ok(Json(AgentDetailDto {
        id: agent_id.to_string(),
        name,
        role,
        description,
        source,
        editable: match is_admin {
            true => EDITABLE_FIELDS.to_vec(),
            false => EDITABLE_FIELDS_MEMBER.to_vec(),
        },
        instructions: effective_instructions,
        blueprint_instructions,
        instructions_overridden,
        tier: declared_tier(record, agent_id),
        harness: declared_harness(record, agent_id),
        model: declared_model(record, agent_id),
        provider: declared_provider(record, agent_id),
        is_orchestrator: is_orchestrator(record, agent_id),
        tools: agent_tools(record, agent_id),
        desks: desks_for(record, agent_id),
        inbox_enabled,
        budget_usd_daily: cap,
        spent_today_usd: spent,
        budget_set_by: attribution.map(|entry| entry.set_by.id.clone()),
        budget_set_at_millis: attribution.map(|entry| entry.at_millis),
        avatar: record.effective_avatar(agent_id),
        mascot_mode: record.effective_mascot_mode(agent_id),
        mascot_costume: record.effective_mascot_costume(agent_id),
        mascot_skin_color: record.effective_mascot_skin_color(agent_id),
        mascot_hand_color: record.effective_mascot_hand_color(agent_id),
    }))
}

/// Every desk this agent is an effective member of, manifest desks first.
///
/// Resolved through
/// [`CompanyRecord::effective_desk_members`](crate::ports::types::CompanyRecord::effective_desk_members)
/// rather than by reading the declared member lists, so an operator-added
/// membership and an operator-set lead order are both reflected — the same
/// answer the Desks page and the harness `desk_lead` resolver give.
///
/// Shared with `GET {scope}/team` (issue #601) for the same anti-drift reason
/// as [`agent_tools`]: desks are the overview graph's departments now, so the
/// roster list and this read have to agree on which desks a teammate sits on.
pub(super) fn desks_for(record: &CompanyRecord, agent_id: &str) -> Vec<AgentDeskDto> {
    let declared = record
        .manifest
        .group_chats
        .iter()
        .map(|chat| (chat.id.as_str(), chat.name.as_str()))
        .chain(
            record
                .overlay_desks
                .iter()
                .map(|desk| (desk.id.as_str(), desk.name.as_str())),
        );
    declared
        .filter_map(|(id, name)| {
            let members = record.effective_desk_members(id);
            members.iter().any(|m| m == agent_id).then(|| AgentDeskDto {
                id: id.to_string(),
                name: name.to_string(),
                // Position is a rank only on a **lead** desk. An `auto`
                // channel (issue #1835) orders its members without conferring
                // anything, so `members[0]` there is whoever happens to be
                // listed first — badging them "(lead)" on TeamView, the agent
                // detail page and the profile sheet states a rank nothing
                // confers (codex on #1872). Read through `desk_lead`, the
                // one definition that is `None` for an auto channel, rather
                // than re-deriving the rule from position here.
                lead: crate::runtime::delegation_tools::desk_lead(record, id).as_deref()
                    == Some(agent_id),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Drafting a mandate or a persona (issue #1776)
// ---------------------------------------------------------------------------

/// What the console asks for when it wants a draft.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DraftRequest {
    /// Which field to draft: `description` or `instructions`.
    ///
    /// Named on the wire rather than inferred, and validated against a closed
    /// set: a request for a field this pass does not draft is refused, not
    /// quietly answered about a different one.
    field: String,
    /// The conversation so far, oldest first — empty on the opening turn.
    ///
    /// The console holds the transcript and sends it back each turn; the host
    /// stores nothing. That is the whole of "in-session": there is no journal
    /// to rehydrate, no thread id to collide, and nothing to clean up when the
    /// operator closes the form.
    ///
    /// Free text from a stranger on both sides, and treated as such all the way
    /// down — framed to the model as a description of what the operator wants
    /// rather than as instructions to it, bounded host-side, and reaching
    /// nothing else.
    #[serde(default)]
    messages: Vec<WireTurn>,
    /// The mandate as it stands **on the operator's screen**, when the console
    /// holds one the record does not.
    ///
    /// The grounding is otherwise read from the record, which is right until
    /// the operator has taken a draft and not saved it yet. Then the two
    /// disagree, and the record is the wrong one to believe: "make it shorter"
    /// has to mean shorter than what they are looking at, not shorter than what
    /// was stored before this conversation began.
    ///
    /// Not a widening. These are the two fields this same request is drafting,
    /// authored on screen right now — the same argument the Add-teammate route
    /// makes for carrying them. Everything else about the company is still
    /// assembled host-side and cannot be influenced from here.
    #[serde(default)]
    description: Option<String>,
    /// The persona as it stands on the operator's screen. See `description`.
    #[serde(default)]
    instructions: Option<String>,
    /// The role as it stands on the operator's screen, when it differs from
    /// the stored one.
    ///
    /// Both prompts are written *from* the role, so this is the field a stale
    /// grounding damages most: an operator who repurposes a teammate and asks
    /// for a mandate before saving gets one written for the job it used to do.
    /// Carried for the same reason as the two fields above and under the same
    /// limit — it is authored on this screen, in this form, right now.
    #[serde(default)]
    role: Option<String>,
    /// The name as it stands on the operator's screen. See `role`.
    #[serde(default)]
    name: Option<String>,
}

/// One turn of a copilot conversation, on the wire.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WireTurn {
    /// `operator` or `copilot`. Anything else drops the turn — see
    /// [`TurnRole::parse`].
    role: String,
    text: String,
}

/// Reads a conversation off the wire, dropping turns whose speaker cannot be
/// established and bounding what survives.
///
/// A dropped turn is deliberately silent rather than a `400`. The transcript is
/// context, not the request: refusing the whole turn because one old message
/// was malformed would lose the operator's actual question, and a conversation
/// missing a line still answers better than no conversation at all.
fn conversation_from(messages: Vec<WireTurn>) -> Vec<CopilotTurn> {
    clamp_conversation(
        messages
            .into_iter()
            .filter_map(|turn| {
                TurnRole::parse(&turn.role).map(|role| CopilotTurn {
                    role,
                    text: turn.text,
                })
            })
            .collect(),
    )
}

/// What the console asks for when it wants a draft for a teammate that does
/// **not exist yet** — the Add-teammate form.
///
/// The teammate's own fields ride the request because there is nowhere else to
/// get them: nothing has been created, so the record holds nothing to ground a
/// draft in. That is not the widening the id-bearing route refuses. These are
/// the very fields being authored on screen right now, and the part that stays
/// host-side is the part that matters — the rest of the company. A caller can
/// describe the teammate it is about to add; it still cannot ask a draft to
/// read anything else.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NewDraftRequest {
    /// Which field to draft: `description` or `instructions`.
    field: String,
    /// The conversation so far, oldest first — empty on the opening turn.
    #[serde(default)]
    messages: Vec<WireTurn>,
    /// The role as typed on the form.
    ///
    /// Required, and the one field a draft cannot proceed without: the role is
    /// what both prompts lean on, and drafting from a blank one would have the
    /// model invent the job before describing it.
    role: String,
    /// The name as typed, when the form has one.
    #[serde(default)]
    name: Option<String>,
    /// The mandate as typed so far, so a persona fits the job the form claims.
    #[serde(default)]
    description: Option<String>,
    /// The persona as typed so far, so a redraft improves on it.
    #[serde(default)]
    instructions: Option<String>,
}

/// One drafted field, for the operator to keep or throw away.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DraftDto {
    /// The field this draft is for, echoed so a late response landing on a form
    /// that has moved on can be matched to the box it was asked for.
    field: &'static str,
    /// What the copilot says in the conversation — what it changed, or what it
    /// needs to know. Absent when the pass refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    reply: Option<String>,
    /// The whole field as it now stands, already clamped to the field's own
    /// bound. Absent when this turn asked a question instead of drafting, and
    /// when the pass refused — `source` tells those apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// `model` when a model wrote this, `unavailable` when none could.
    ///
    /// The console says which. Rendering a refusal and a draft identically is
    /// the failure the roster review screen already avoids: someone shown
    /// nothing with no reason assumes the feature is broken, and someone shown
    /// canned text assumes a model read their company.
    source: &'static str,
    /// Why there is no draft. Present only when `source` is `unavailable`, and
    /// distinct per cause because the operator's next move differs: wire up a
    /// model, retry the provider, or say more.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl DraftDto {
    fn from_draft(field: ProfileField, draft: ProfileDraft) -> Self {
        match draft {
            ProfileDraft::Answered { reply, draft } => Self {
                field: field.as_str(),
                reply: Some(reply),
                text: draft,
                source: "model",
                reason: None,
            },
            ProfileDraft::Refused(reason) => Self {
                field: field.as_str(),
                reply: None,
                text: None,
                source: "unavailable",
                reason: Some(reason.as_str()),
            },
        }
    }
}

/// `POST {scope}/team/{agent_id}/draft` — draft this teammate's mandate or
/// persona (issue #1776).
///
/// # This route never writes
///
/// It loads the record, composes a prompt from it, and returns text. Nothing is
/// stored: not the draft, not the hint, not the fact that one was asked for.
/// The company record is byte-identical afterwards, which is why it takes no
/// write lock and why a draft cannot lose a concurrent edit.
///
/// That is the whole reason a model may write into these two fields at all.
/// [`crate::company::setup`] keeps the roster designer out of a teammate's
/// standing instructions because there the text reaches a system prompt with
/// nobody having read it; here the operator reads it, chooses to keep it, and
/// then saves it through [`edit_agent`] like any other edit they typed. Two
/// deliberate human actions stand between this response and a running persona,
/// and if either is ever removed this route has to be reconsidered with it.
///
/// # Who may ask
///
/// Any signed-in member, matching the `PATCH` for the fields it drafts:
/// `description` and `instructions` are member-open there, so a draft of them
/// cannot sensibly be admin-only. It is deliberately *not* wider than the
/// write it feeds — a caller who could draft a persona but not save one would
/// only be able to spend the company's tokens.
///
/// # Refusals
///
/// An unknown id is a `404`, exactly as the `GET` and `PATCH` on this path.
/// An unknown field is a `400`. Everything else — no model wired, a provider
/// that did not answer, an answer that could not be read — is a `200` carrying
/// a reason, because none of those is a failure of the *request*: the operator
/// asked a reasonable thing and the honest answer is "not right now, here's
/// why". An error status would put a red banner over a form that is working
/// fine, and would tell them nothing about which of the three happened.
pub(super) async fn draft_profile(
    company: ScopedCompany,
    State(_state): State<AppState>,
    Path(AgentPath { agent_id }): Path<AgentPath>,
    Json(body): Json<DraftRequest>,
) -> Result<Json<DraftDto>, ApiError> {
    let Some(field) = ProfileField::parse(&body.field) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "`{}` is not a draftable field; expected `description` or `instructions`",
            body.field
        ))));
    };

    let record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;

    let on_screen = InProgress {
        description: body.description,
        instructions: body.instructions,
        // Identity is short and single-line, so it takes the one-line bound
        // rather than a field's own; what matters is that it takes one at all.
        role: blank_to_none(body.role.as_deref().map(clamp_description)),
        name: blank_to_none(body.name.as_deref().map(clamp_description)),
    };
    let subject = subject_for(
        &record,
        &agent_id,
        conversation_from(body.messages),
        on_screen,
    )
    .ok_or_else(|| {
        ApiError(OpenCompanyError::CompanyNotFound(format!(
            "teammate {agent_id}"
        )))
    })?;

    let turns = subject.conversation.len();
    let draft = build_draft(&company, &record, field, &subject).await;
    tracing::info!(
        company = %company.id(),
        agent = %agent_id,
        field = field.as_str(),
        turns,
        // Three outcomes, not two: a turn that asked a question drafted
        // nothing and is not a failure, and logging it as one would make a
        // working copilot look broken in the log.
        outcome = match draft.refusal().map(|r| r.as_str()) {
            Some(reason) => reason,
            None if draft.text().is_some() => "drafted",
            None => "asked",
        },
        "[draft] answered a teammate profile turn"
    );
    Ok(Json(DraftDto::from_draft(field, draft)))
}

/// What the reduced Add-teammate dialog sends to have a teammate designed
/// (issue #1989): a name, and the one sentence the operator typed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DesignRequest {
    /// The name the operator gave, which nothing can derive and nothing here
    /// changes. Carried only as grounding, so the mandate can address the
    /// teammate as they will.
    #[serde(default)]
    name: Option<String>,
    /// What the operator said this teammate should do. The whole input.
    description: String,
}

/// A teammate as one design pass wrote it, or the reason there is none.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DesignDto {
    /// The job title. Absent when the pass refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    /// The mandate. Absent when the pass refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// The standing instructions. Absent when the pass refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    /// `model` when a model designed this, `unavailable` when none could.
    source: &'static str,
    /// Why there is none. Present only when `source` is `unavailable`, and one
    /// of the four [`DraftRefusal`] spellings — the console says a different
    /// sentence for each, because "wire up a model", "try again", "say more"
    /// and "wait for the period to reset" are four different next moves.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl DesignDto {
    fn from_designed(designed: DesignedTeammate) -> Self {
        match designed {
            DesignedTeammate::Designed(design) => Self {
                role: Some(design.role),
                description: Some(design.description),
                instructions: Some(design.instructions),
                source: "model",
                reason: None,
            },
            DesignedTeammate::Refused(reason) => Self {
                role: None,
                description: None,
                instructions: None,
                source: "unavailable",
                reason: Some(reason.as_str()),
            },
        }
    }
}

/// `POST {scope}/team/design` — design a whole teammate from a name and a
/// sentence (issue #1989).
///
/// # Why this route exists at all
///
/// The reduced Add-teammate dialog collects a name and one sentence. Something
/// has to turn that into the three fields a teammate is actually made of, and
/// until this route the console did it by **splitting the sentence**: first
/// clause for the role, cut at sixty characters with an ellipsis. That shipped
/// teammates whose stored job title was `"Runs wholesale outreach to boutique
/// retailers and keeps the…"`, interpolated verbatim into `persona_prompt` and
/// rendered beside their id in the orchestrator's Team block. A string split
/// cannot tell a job from an adverbial, and no amount of tuning makes it able
/// to; a model reading the sentence can.
///
/// # Why drafting a role is allowed here and nowhere else
///
/// `POST {scope}/team/{agent_id}/draft` still refuses anything but
/// `description` and `instructions`, and must keep refusing. Its reason — a
/// role is what delegation grounds on, so a drafted one would change who the
/// company routes work to — is a statement about **editing a teammate that
/// exists**: work is already addressed to it, and a model re-pointing that is
/// the harm.
///
/// This route takes **no agent id**. There is no teammate yet, nothing is
/// routed to it, and no orchestrator has seen it, so the property that
/// exclusion protects is not in play. The separation is structural rather than
/// a flag: there is no request shape that reaches this pass carrying an
/// existing teammate's id, so it cannot rewrite one.
///
/// # This route never writes
///
/// Like the two draft routes, and for the same reason: it loads the record,
/// composes a prompt from it, and returns text. The console writes the teammate
/// afterwards through `POST {scope}/team`, which validates what it is given.
/// The record is byte-identical when this returns, so it takes no write lock.
///
/// # Who may ask
///
/// Any signed-in member, matching `POST {scope}/team` — the write this feeds.
/// Wider would let a caller spend the company's tokens on a teammate they could
/// not then create.
///
/// # Refusals
///
/// A blank description is a `400`: it is the entire input, and designing from
/// nothing is a model inventing a job rather than reading one. Everything else
/// — no model wired, a provider that did not answer, an answer that could not
/// be read, the plan's token ceiling reached — is a `200` carrying a reason,
/// exactly as the draft routes answer, because none of those is a failure of
/// the request. The console's answer to all four is the same: show the full
/// form, carrying what was typed, and let the operator write the fields
/// themselves.
pub(super) async fn design_teammate(
    company: ScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<DesignRequest>,
) -> Result<Json<DesignDto>, ApiError> {
    let description = body.description.trim();
    if description.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "say what the teammate should do — a design is written from it".to_string(),
        )));
    }

    let record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;

    let subject = ProfileSubject {
        company_name: record.manifest.company.name.clone(),
        company_output: record.manifest.company.output.clone(),
        // No id, and none invented: "the one being added" is what an empty id
        // means here, exactly as on `POST {scope}/team/draft`.
        agent_id: String::new(),
        name: blank_to_none(body.name.as_deref().map(clamp_description)),
        // Empty, and this is the point of the route: the role is what the pass
        // produces, not what grounds it.
        role: String::new(),
        // Bounded by prompt weight, NOT by the roster card. This used to be
        // `ProfileField::Description.clamp`, which is `MAX_DESCRIPTION` — a
        // *layout* bound, 200 characters, because a card has one line for a
        // mandate. Applied to the operator's brief it cut their sentence at 200
        // with an `…` on the end before the model ever read it, and nothing
        // said so: the console's box had no limit, and the record stores the
        // model's description rather than the operator's, so a requirement
        // written past character 200 vanished without a trace. See
        // `MAX_DESIGN_BRIEF`; the console holds the same number on the box.
        description: Some(clamp_design_brief(description)),
        instructions: None,
        // Ids and roles only, the same closed grounding every draft gets, so a
        // designed teammate does not duplicate a job the company already has.
        siblings: siblings_of(&record, ""),
        // A design is one shot. There is no conversation to carry, and adding
        // one would make this the draft route with a wider field list.
        conversation: Vec::new(),
    };

    // Armed across the one await a disconnect can land inside. See `DesignSeam`.
    let mut seam = DesignSeam {
        company: company.id().to_string(),
        finished: false,
    };
    let designed = build_design(&company, &record, &subject).await;
    seam.finished = true;
    tracing::info!(
        company = %company.id(),
        outcome = designed.refusal().map(|r| r.as_str()).unwrap_or("designed"),
        "[design] answered a teammate design request"
    );
    Ok(Json(DesignDto::from_designed(designed)))
}

/// Makes an abandoned design pass loud instead of silent.
///
/// A design pass is the one place in this file where the caller can walk away
/// mid-flight: the console's Add-teammate dialog aborts the request when it is
/// shut, and `axum` drops this handler's future when the socket closes. That
/// drop lands **between** the provider call and `record_profile_draft_usage`,
/// so the `DraftBudget` reservation is released and nothing is recorded — while
/// whatever the provider had already generated may still have been billed.
///
/// This does not fix that; it makes it countable. Deciding what a cancelled
/// pass *should* cost is a product question with two defensible answers — let
/// the pass finish so real usage is recorded (and abandoning it saves nothing),
/// or charge a conservative estimate on the way out (and over-charge a pass
/// cancelled after 200ms) — and neither should be picked silently inside a
/// review cycle. Issue #2138 carries that decision; until it is made, a grep
/// for this line is how the size of the gap gets measured rather than guessed
/// at.
struct DesignSeam {
    company: String,
    finished: bool,
}

impl Drop for DesignSeam {
    fn drop(&mut self) {
        if !self.finished {
            tracing::warn!(
                company = %self.company,
                "[design] the caller went away before the pass finished — the budget \
                 reservation is released and any provider work already done is not metered"
            );
        }
    }
}

/// Runs the design pass, reserving its ceiling first.
///
/// The same order and the same reasoning as [`build_draft`]: a company with
/// nothing wired has a truer answer than "out of budget", and a company that is
/// out of budget must not reach the provider at all.
#[cfg(feature = "openhuman")]
async fn build_design(
    company: &ScopedCompany,
    record: &CompanyRecord,
    subject: &ProfileSubject,
) -> DesignedTeammate {
    let Some(drafter) = company.runtime.profile_drafter() else {
        return DesignedTeammate::Refused(DraftRefusal::NoModel);
    };
    let Some(_budget) = reserve_draft_budget(
        company.id(),
        company.runtime.usage().as_ref(),
        &record.manifest.plan,
        crate::harness::profile_draft::design_output_ceiling(),
    )
    .await
    else {
        return DesignedTeammate::Refused(DraftRefusal::BudgetExhausted);
    };
    let provider = drafter.provider_slug();
    let (designed, usage) = drafter.design(subject).await;
    let model = drafter.model_slug();
    crate::metering::record_profile_draft_usage(
        &usage,
        &provider,
        model,
        company.id(),
        company.runtime.store().as_ref(),
        company.runtime.usage().as_ref(),
    )
    .await;
    designed
}

/// The default build links no harness, so there is no model to design with and
/// saying so is the whole answer.
#[cfg(not(feature = "openhuman"))]
async fn build_design(
    _company: &ScopedCompany,
    _record: &CompanyRecord,
    _subject: &ProfileSubject,
) -> DesignedTeammate {
    DesignedTeammate::Refused(DraftRefusal::NoModel)
}

/// `POST {scope}/team/draft` — draft a field for a teammate the operator is
/// still filling in (issue #1776).
///
/// The Add-teammate form's entry point. Same contract as
/// [`draft_profile`] in every way that matters — it writes nothing, it is open
/// to the same members, and its refusals are the same three reasons — and
/// differs only in where the teammate's own fields come from, because there is
/// no teammate yet to read them off.
///
/// `/team/draft` is a static segment, so it cannot be confused with a teammate
/// whose id happens to be `draft`: nothing serves `POST` on
/// `/team/{agent_id}`, and that teammate's own drafting path would be
/// `/team/draft/draft`.
///
/// A blank `role` is a `400`. It is the one field both prompts lean on, and a
/// draft written from an empty role is a model inventing the job before
/// describing it — the console disables the control for the same reason, so
/// this is the host stating the rule rather than trusting it to.
pub(super) async fn draft_new_profile(
    company: ScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<NewDraftRequest>,
) -> Result<Json<DraftDto>, ApiError> {
    let Some(field) = ProfileField::parse(&body.field) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "`{}` is not a draftable field; expected `description` or `instructions`",
            body.field
        ))));
    };
    let role = body.role.trim();
    if role.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "give the teammate a role before drafting — a draft is written from it".to_string(),
        )));
    }

    let record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;

    let conversation = conversation_from(body.messages);
    let subject = ProfileSubject {
        company_name: record.manifest.company.name.clone(),
        company_output: record.manifest.company.output.clone(),
        // No id yet, and none invented. The subject carries it only so a draft
        // can be told which teammate it is about, and "the one being added" is
        // what an empty id means here.
        agent_id: String::new(),
        // Every one of these four arrives from the caller, and on this route
        // nothing else has bounded them: the teammate does not exist, so there
        // is no stored record that already passed the field's own limit. Only
        // the request body cap stands between a pasted document and the
        // prompt, which is a ceiling measured in megabytes rather than in what
        // the field can hold. Each is clamped to the bound it would have to
        // obey to be *saved*, so a grounding loses nothing that could have
        // become the teammate.
        //
        // A field that is blank once trimmed is dropped rather than sent as an
        // empty string, matching what `InProgress::or_stored` does for the
        // teammate that already exists: "" is not a mandate, and putting one in
        // the prompt tells the model this teammate HAS an empty mandate rather
        // than none yet.
        name: blank_to_none(body.name.as_deref().map(clamp_description)),
        role: clamp_description(role),
        description: blank_to_none(
            body.description
                .as_deref()
                .map(|text| ProfileField::Description.clamp(text)),
        ),
        instructions: blank_to_none(
            body.instructions
                .as_deref()
                .map(|text| ProfileField::Instructions.clamp(text)),
        ),
        // Every teammate on the roster is a sibling of one that is not on it
        // yet, so nothing is filtered out — and this is exactly when the list
        // earns its keep: a mandate written for a teammate about to be added is
        // the one most likely to restate a job the company already has.
        siblings: siblings_of(&record, ""),
        conversation,
    };

    let turns = subject.conversation.len();
    let draft = build_draft(&company, &record, field, &subject).await;
    tracing::info!(
        company = %company.id(),
        field = field.as_str(),
        turns,
        outcome = match draft.refusal().map(|r| r.as_str()) {
            Some(reason) => reason,
            None if draft.text().is_some() => "drafted",
            None => "asked",
        },
        "[draft] answered a turn for a teammate being added"
    );
    Ok(Json(DraftDto::from_draft(field, draft)))
}

/// A field that is blank once clamped is no field at all.
///
/// The Add form sends every box it has, including the ones the operator has
/// not filled in, so `Some("")` reaches here routinely. Passed on, it tells
/// the model this teammate *has* an empty mandate rather than none yet — a
/// difference the prompt is written around. `InProgress::or_stored` makes the
/// same call for the teammate that already exists.
fn blank_to_none(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

/// The authored fields as the console currently shows them, when it has
/// something the record does not.
///
/// Four rather than two. The role and the name are as edit-in-progress as the
/// mandate and the persona — the same form holds all four — and the role is
/// the one a stale grounding hurts most, since both prompts are written from
/// it: a teammate repurposed on screen and drafted for before saving gets a
/// mandate for the job it used to do.
#[derive(Debug, Default)]
pub(super) struct InProgress {
    pub(super) description: Option<String>,
    pub(super) instructions: Option<String>,
    /// Already clamped and blank-normalised by the handler, unlike the two
    /// prose fields, which are clamped per-field inside [`Self::or_stored`].
    pub(super) role: Option<String>,
    /// See [`Self::role`].
    pub(super) name: Option<String>,
}

impl InProgress {
    /// The on-screen value where there is one, else what was stored.
    ///
    /// A blank on-screen value is NOT a value: the operator clearing the box is
    /// them about to write something, not an instruction to the copilot that
    /// the field is now empty. Falling back keeps the draft grounded in the
    /// last thing anyone actually wrote.
    ///
    /// The on-screen value is clamped to the bound `field` itself obeys, which
    /// the stored one has already passed on its way in. It arrives from the
    /// caller and nothing else has bounded it: the request body limit is the
    /// only ceiling on the way here, and a megabyte of pasted text would go
    /// into the prompt — and onto the bill — unread. Clamping to the field's
    /// own bound costs a grounding nothing, because text past that bound could
    /// never have been saved into the field anyway.
    fn or_stored(
        field: ProfileField,
        on_screen: Option<String>,
        stored: Option<String>,
    ) -> Option<String> {
        on_screen
            .map(|text| field.clamp(&text))
            .filter(|text| !text.trim().is_empty())
            .or(stored)
    }
}

/// Everything a draft is allowed to see about the teammate it is for.
///
/// Assembled here, from the record, rather than accepted from the caller. The
/// console holds all of this already and could have sent it, and that is
/// exactly why it must not: a grounding the caller composes is a grounding the
/// caller can widen, and this one is deliberately narrow — this teammate, its
/// neighbours' ids and roles, and nothing else about the company.
///
/// `None` when the id names nobody on the roster.
fn subject_for(
    record: &CompanyRecord,
    agent_id: &str,
    conversation: Vec<CopilotTurn>,
    on_screen: InProgress,
) -> Option<ProfileSubject> {
    // The same two halves `detail` resolves, in the same order: a manifest row
    // with the operator's edits applied wins an id collision, exactly as
    // `build_roster` resolves one.
    let manifest_agent = record.effective_agent(agent_id);
    let overlay_agent = record.overlay_agents.iter().find(|a| a.id == agent_id);
    let (name, role, description) = match (manifest_agent.as_deref(), overlay_agent) {
        (Some(agent), _) => (
            agent.name.clone(),
            agent.role.clone(),
            agent.description.clone(),
        ),
        (None, Some(agent)) => (
            Some(agent.name.clone()),
            agent.role.clone(),
            agent.description.clone(),
        ),
        (None, None) => return None,
    };

    Some(ProfileSubject {
        company_name: record.manifest.company.name.clone(),
        company_output: record.manifest.company.output.clone(),
        agent_id: agent_id.to_string(),
        // On-screen identity wins over stored identity for the same reason the
        // prose fields do: the operator is drafting for the teammate in front
        // of them, not the one that was saved. Already bounded by the handler.
        name: on_screen.name.or(name),
        role: on_screen.role.unwrap_or(role),
        description: InProgress::or_stored(
            ProfileField::Description,
            on_screen.description,
            description,
        ),
        // The persona in force — the override where one is set, else the
        // blueprint seed — so a redraft improves on what the teammate actually
        // runs on rather than on what its manifest row happened to say. Unless
        // the operator is looking at something newer, which wins.
        instructions: InProgress::or_stored(
            ProfileField::Instructions,
            on_screen.instructions,
            record.effective_instructions(agent_id),
        ),
        siblings: siblings_of(record, agent_id),
        conversation,
    })
}

/// Every other teammate on the roster, id and role only.
///
/// Manifest teammates first and then overlay ones, the order
/// [`super::team`]'s list read uses, so the roster a draft is told about is the
/// roster an operator sees.
///
/// Id **and** role, because both are load-bearing and for different reasons:
/// the role is what a mandate must not restate, and the id is what the
/// delegation surface actually prints beside it (issue #1162) — two teammates
/// the company cannot tell apart is the failure this list exists to prevent.
fn siblings_of(record: &CompanyRecord, agent_id: &str) -> Vec<Sibling> {
    record
        .effective_agents()
        .into_iter()
        .map(|agent| Sibling {
            id: agent.id,
            role: agent.role,
        })
        .chain(record.overlay_agents.iter().map(|agent| Sibling {
            id: agent.id.clone(),
            role: agent.role.clone(),
        }))
        .filter(|sibling| sibling.id != agent_id)
        .collect()
}

/// Whether the tenant's plan-level token ceiling (issue #188) has already been
/// reached, in which case no draft may run.
///
/// A draft is a completion the tenant pays for, and
/// [`tokens_in`] counts [`SampleKind::AuthoringCall`](crate::ports::usage::SampleKind::AuthoringCall)
/// toward that ceiling — so without this the ceiling is one the copilot only
/// *contributes* to and never obeys. Drafting is operator-driven and
/// repeatable by the same click, so a member past the cap could keep spending
/// through this route indefinitely while every other dispatch is refused.
///
/// This is the same gate `run_inner`'s `total_ceiling_refusal` applies before
/// harness dispatch, and it fails the same way it does — an unreadable meter
/// or an absent one **warns and lets the draft through** rather than refusing.
/// A metering outage that silently disabled a working copilot would be a worse
/// failure than a draft or two past the line, and the per-namespace roster is
/// fail-closed independently.
///
/// Takes the meter and the manifest plan rather than the [`ScopedCompany`] it
/// is called with, so the rule can be exercised against a meter that reports a
/// known spend — the gate is worth nothing if the only way to see it work is a
/// live tenant that has already overspent.
// Compiled where it can run: the drafting pass itself is behind `openhuman`,
// and `test` so the default lane still exercises the rule.
#[cfg(any(feature = "openhuman", test))]
async fn reserve_draft_budget(
    company: &crate::ports::types::CompanyId,
    meter: &dyn crate::ports::UsageMeter,
    manifest_plan: &crate::company::Plan,
    tokens: u32,
) -> Option<Option<crate::metering::DraftBudget>> {
    use crate::metering::{CapabilityPlan, tokens_in};

    let Some(plan) = CapabilityPlan::from_manifest(manifest_plan) else {
        return Some(None);
    };
    // No ceiling configured is the common case, and asking the meter about it
    // would put a usage query in front of every draft for nothing — nor is
    // there anything to promise against.
    if plan.total_budget.is_none() {
        return Some(None);
    }
    let since = plan.period.period_start_millis(crate::ports::now_millis());
    match meter.query(company, since).await {
        Ok(samples) => {
            let spent = tokens_in(&samples);
            // The check and the promise happen together, under the reservation
            // map's own lock. Reading the meter here and deciding there would
            // leave the same gap this exists to close: the meter can only
            // report finished work, and two drafts a click apart are both
            // unfinished.
            match crate::metering::reserve_draft(company, u64::from(tokens), spent, &plan) {
                Some(budget) => Some(Some(budget)),
                None => {
                    tracing::info!(
                        company = %company,
                        spent,
                        "[draft] total token ceiling reached; refusing to draft (no model call) until the period resets"
                    );
                    None
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                company = %company,
                %error,
                "[draft] total-ceiling spend query failed; not refusing the draft"
            );
            Some(None)
        }
    }
}

/// The draft itself: written by a model when one is wired, refused with a
/// reason when none is.
///
/// The two arms are not a happy path and a degraded one — a company with no
/// inference credential is a supported configuration. What it is *not* is a
/// company that should be handed canned text: there is no curated fallback for
/// "what does this particular teammate own", the way there is for a starting
/// roster, so the honest answer is the refusal and the operator writes the
/// field themselves.
#[cfg(feature = "openhuman")]
async fn build_draft(
    company: &ScopedCompany,
    record: &CompanyRecord,
    field: ProfileField,
    subject: &ProfileSubject,
) -> ProfileDraft {
    let Some(drafter) = company.runtime.profile_drafter() else {
        return ProfileDraft::Refused(DraftRefusal::NoModel);
    };
    // Checked after the drafter and before the call: a company with nothing
    // wired has a truer answer to give than "out of budget", and a company that
    // is out of budget must not reach the provider at all.
    //
    // The promise is held across the call and dropped with `_budget` when this
    // function returns, on every path — including the ones that never reached a
    // provider.
    let Some(_budget) = reserve_draft_budget(
        company.id(),
        company.runtime.usage().as_ref(),
        &record.manifest.plan,
        crate::harness::profile_draft::output_ceiling(field),
    )
    .await
    else {
        return ProfileDraft::Refused(DraftRefusal::BudgetExhausted);
    };
    let provider = drafter.provider_slug();
    let (draft, usage) = drafter.draft(field, subject).await;
    // Read *after* the turn, so it names the model the turn actually ran on —
    // the same ordering the roster pass uses (issue #1749).
    let model = drafter.model_slug();
    // Metered whatever came back: an unreadable answer was still billed, and a
    // refusal that never reached a provider moved no tokens and writes no row.
    crate::metering::record_profile_draft_usage(
        &usage,
        &provider,
        model,
        company.id(),
        company.runtime.store().as_ref(),
        company.runtime.usage().as_ref(),
    )
    .await;
    draft
}

/// The default build links no harness, so there is no model to draft with and
/// saying so is the whole answer.
#[cfg(not(feature = "openhuman"))]
async fn build_draft(
    _company: &ScopedCompany,
    _record: &CompanyRecord,
    _field: ProfileField,
    _subject: &ProfileSubject,
) -> ProfileDraft {
    ProfileDraft::Refused(DraftRefusal::NoModel)
}

#[cfg(test)]
#[path = "team_agent_test_support.rs"]
mod team_agent_test_support;
#[cfg(test)]
#[path = "team_agent_a_company_past_its_tests.rs"]
mod tests_a_company_past_its;
#[cfg(test)]
#[path = "team_agent_a_manifest_teammates_tools_tests.rs"]
mod tests_a_manifest_teammates_tools;
#[cfg(test)]
#[path = "team_agent_a_member_may_change_tests.rs"]
mod tests_a_member_may_change;
#[cfg(test)]
#[path = "team_agent_harness_and_model_persist_tests.rs"]
mod tests_harness_and_model_persist;
#[cfg(test)]
#[path = "team_agent_requested_grants_reads_overlay_tests.rs"]
mod tests_requested_grants_reads_overlay;
#[cfg(test)]
#[path = "team_agent_the_roster_list_carries_tests.rs"]
mod tests_the_roster_list_carries;
