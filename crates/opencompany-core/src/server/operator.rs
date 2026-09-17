//! Operator HTTP surface: chat with a company and resolve its approvals.
//!
//! Phase 1 ships synchronous JSON chat: a `POST .../chat` enqueues an
//! `OperatorMessage`, runs exactly one cycle, and returns the channel
//! responses. SSE streaming (`/chat` streaming plus a `GET /events` work feed)
//! is the first follow-up.
//!
//! Both addressing forms are served by one router: the platform `{id}` form and
//! the prosumer single-company aliases (`/api/v1/company/...`) resolved through
//! [`CompanyRegistry::sole`](crate::runtime::CompanyRegistry::sole).
//!
//! Auth is a platform token (hosting layer) or a human's session cookie; there
//! is no unauthenticated path. See [`server::users`](crate::server::users).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures::StreamExt;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::blockers::BlockerVerdict;
use crate::ports::events::EventStreamItem;
use crate::ports::store::company_write_lock;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, Attachment, CompanyEvent, CompanyId, CompanyRecord, EventSeq,
    OutboundMessage, OverlayDesk, OverlayDeskMember, OverlayDeskOrder, ResponderMode, StoredEvent,
    TurnStep, Verdict,
};
use crate::runtime::cycle::ResolveReceipt;
use crate::runtime::grants::{GrantId, GrantScope, MAX_STANDING_GRANT_MILLIS};
use crate::runtime::types::{ApprovalSummary, CompanyStatus, CycleReport};
use crate::server::chat_history::{
    CHAT_HISTORY_PAGE_LIMIT, MentionView, MessageView, ReactionView, Viewer, author_labels,
    channel_attributed_replies, history_for_desk, project_mentions,
};
use crate::server::error::ApiError;
use crate::server::graphql::auth::GqlAuth;
use crate::server::ops::language::{self, DEFAULT_DESK};
use crate::server::ops::{AdminScopedCompany, ScopedCompany, scoped};
use crate::server::platform_auth::{CompanyAuth, authorize_address, refuse_until_password_changed};
use crate::server::provision::{emit_cycle_webhooks, emit_feedback_webhook};

/// Builds the operator route fragment, merged into the main router.
pub fn router() -> Router<AppState> {
    let router = Router::new()
        .route("/api/v1/companies", get(list_companies))
        .route("/api/v1/companies/{id}", get(company_status))
        .route("/api/v1/companies/{id}/chat", post(operator_chat))
        .route("/api/v1/companies/{id}/chat/history", get(chat_history))
        // One agent's whole session: every line it said and heard, across every
        // channel it can read, in journal order. Registered explicitly rather
        // than through `scoped` because the two forms take different path
        // tuples — `(id, agent_id)` against `(agent_id)`.
        .route(
            "/api/v1/companies/{id}/agents/{agent_id}/session",
            get(agent_session),
        )
        .route(
            "/api/v1/company/agents/{agent_id}/session",
            get(agent_session_single),
        )
        .route(
            "/api/v1/companies/{id}/chat/attribution-audit",
            get(attribution_audit),
        )
        // Set or clear one reaction on one message (issue #364). Not registered
        // through `scoped` because the two forms take different path tuples.
        .route(
            "/api/v1/companies/{id}/chat/messages/{seq}/reactions",
            post(react_to_message_scoped),
        )
        .route("/api/v1/companies/{id}/approvals", get(list_approvals))
        // Single-company aliases (no id; resolved via the sole registered company).
        .route("/api/v1/company/chat", post(operator_chat_single))
        .route("/api/v1/company/chat/history", get(chat_history_single))
        .route(
            "/api/v1/company/chat/attribution-audit",
            get(attribution_audit_single),
        )
        .route(
            "/api/v1/company/chat/messages/{seq}/reactions",
            post(react_to_message_single),
        )
        .route("/api/v1/company/approvals", get(list_approvals_single))
        // Deciding an approval, and extending the deadline that would otherwise
        // decide it by default, settle an effect for the whole company, so both
        // demand authority over it rather than membership in it. Registered
        // through `scoped` so the two address forms cannot drift apart.
        .merge(scoped("/approvals/{aid}", post(resolve_approval)))
        .merge(scoped("/approvals/{aid}/extend", post(extend_approval)))
        // The company's desks (group chats), under both scope forms — the
        // console builds its chat threads from these (issue #53). `POST` creates
        // a desk through the operator overlay (the manifest is never rewritten).
        .merge(scoped("/desks", get(list_desks).post(create_desk)))
        // Delete an operator-created desk (a manifest desk is part of the
        // blueprint and cannot be deleted here).
        .merge(scoped("/desks/{desk_id}", delete(delete_desk)))
        // Desk membership writes (issue #72): add an agent to a desk, or remove
        // an operator-added member. Registered under both scope forms.
        .merge(scoped("/desks/{desk_id}/members", post(add_desk_member)))
        .merge(scoped(
            "/desks/{desk_id}/members/{agent_id}",
            delete(remove_desk_member),
        ))
        // A desk's move grammar: read the table in force, install or replace it,
        // or drop the override and fall back to the manifest's own block.
        // Registered under both scope forms.
        .merge(scoped(
            "/desks/{desk_id}/hive",
            get(desk_hive).put(set_desk_hive).delete(reset_desk_hive),
        ))
        // Desk member ordering / hierarchy (issue #131): set the operator's
        // explicit member order for a desk. Registered under both scope forms.
        .merge(scoped("/desks/{desk_id}/order", put(set_desk_order)))
        // The always-present, durable Operator feed — its own surface, not a
        // desk (issue #1757 rework). Read-only identity lookup: the console
        // pins it below a divider in the chat rail rather than folding it
        // into `GET {scope}/desks`.
        .merge(scoped("/operator-channel", get(operator_channel)))
        // The company → operator attention feed (issue #66): a live SSE stream of
        // the attention-worthy events already on the company's event log, under
        // both scope forms.
        .merge(scoped("/events", get(company_events)))
        // Standing permissions (issue #374): what the operator has opened up,
        // and how to take it back. Registered under both scope forms.
        .merge(scoped("/grants", get(list_grants)))
        .merge(scoped("/grants/{gid}", delete(revoke_grant)));
    with_review_routes(router)
}

/// Registers the thread-scoped review verdict route — Approve finishes a
/// settled `in_review` dispatch card, Revise re-runs it. Gated with the harness
/// that dispatches cards in the first place; the default build has no such card
/// to review, so the route is not mounted.
#[cfg(feature = "openhuman")]
fn with_review_routes(router: Router<AppState>) -> Router<AppState> {
    router.merge(scoped("/chat/review", post(review_card)))
}

#[cfg(not(feature = "openhuman"))]
fn with_review_routes(router: Router<AppState>) -> Router<AppState> {
    router
}

/// One desk (group chat) as the console renders it. Mirrors `DeskDto` in
/// `frontend/src/api/types.ts`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeskDto {
    /// The desk id (the group-chat id; used as the chat thread id).
    id: String,
    /// The desk's display name.
    name: String,
    /// An optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// The effective teammate ids on this desk — the manifest's members unioned
    /// with operator-added overlay members (issue #72), then re-ordered by the
    /// operator's desk hierarchy if one is set (issue #131). The first is the
    /// desk lead. The order carries the hierarchy, so no separate field is
    /// needed; a reorder is written through `PUT {scope}/desks/{id}/order`.
    members: Vec<String>,
    /// The subset of `members` added through the operator overlay, so the
    /// console can offer a remove action for those (manifest members are part of
    /// the blueprint and cannot be removed at runtime). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    overlay_members: Vec<String>,
    /// How this desk's unmentioned messages find their answerer (issue #1835):
    /// `"lead"` — `members[0]` leads and answers — or `"auto"`, a channel with
    /// **no lead**, whose answerer is picked per message by best fit over the
    /// membership. Omitted when `lead` (which is every manifest desk and every
    /// desk created before the field existed), so old consoles and old wire
    /// shapes are byte-for-byte unchanged. The console reads this to suppress
    /// every lead affordance — crown, badge, Make-lead — on `auto` channels.
    #[serde(skip_serializing_if = "ResponderMode::is_lead")]
    responder: ResponderMode,
    /// Whether the whole desk was operator-created (an overlay desk) rather than
    /// declared in the manifest blueprint. The console offers a delete action
    /// only for these — blueprint desks cannot be deleted at runtime. Omitted
    /// (defaults false) for manifest desks.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    overlay_created: bool,
}

/// `GET {scope}/desks` — the company's desks, built from its manifest group
/// chats with any operator-added overlay members merged in (issue #72). Empty
/// when the company defines none (the console then falls back to its static
/// default threads).
async fn list_desks(scope: ScopedCompany) -> Result<Json<Vec<DeskDto>>, crate::server::Rejection> {
    let record = scope.runtime.store().load(scope.id()).await?;
    let desks = record
        .map(|record| {
            // Manifest (blueprint) desks first, then operator-created overlay
            // desks — the same order the harness `desk_lead` resolver searches.
            // The general desk is not listed beside General — it IS General.
            // `[company].general_desk` names the desk the company's own line
            // resolves to, so projecting it as its own channel puts the same
            // room in the sidebar twice: once as the main thread everybody
            // already has, once under whatever id the manifest gave it. Same
            // reasoning, and same `is_general_chat` shape, as the overlay-desk
            // exclusion below.
            let general_desk = record.manifest.company.general_desk.clone();
            let manifest_desks = record
                .manifest
                .group_chats
                .iter()
                .filter(move |chat| general_desk.as_deref() != Some(chat.id.as_str()))
                .map(|chat| {
                    let members = record.effective_desk_members(&chat.id);
                    // The overlay subset: effective members not declared in the
                    // manifest for this desk.
                    let overlay_members = members
                        .iter()
                        .filter(|m| !chat.members.contains(m))
                        .cloned()
                        .collect();
                    DeskDto {
                        id: chat.id.clone(),
                        name: chat.name.clone(),
                        description: chat.description.clone(),
                        members,
                        overlay_members,
                        // Manifest desks are always lead-routed — the blueprint
                        // syntax carries no responder field (issue #1835).
                        responder: ResponderMode::Lead,
                        overlay_created: false,
                    }
                });
            // An overlay desk whose own **id** is a General spelling is not
            // projected (issue #1781 review, Codex P2) — the grandfathered
            // shape `POST .../desks` accepted `general` / `main` ids under
            // before issue #1743 reserved them. `CompanyRecord::resolve_desk_id`
            // already excludes exactly this desk from routing (see its own
            // filter, same `is_general_chat(Some(&d.id))` check), so listing it
            // here would show the console a desk `buildChannels` treats as the
            // company-wide line — offering edit/delete controls and a member
            // list that has nothing to do with where a message to it actually
            // routes (the built-in `#general`, per `resolve_desk_id`'s
            // fallback). Nothing is lost by hiding it: its transcript is
            // already folded into `#general` by `is_general_chat`, and that
            // channel's membership is the whole roster, a superset of whatever
            // this desk held.
            let overlay_desks = record
                .overlay_desks
                .iter()
                .filter(|desk| !crate::server::chat_history::is_general_chat(Some(&desk.id)))
                .map(|desk| {
                    let members = record.effective_desk_members(&desk.id);
                    // For an overlay desk the founding members are `desk.members`;
                    // anything beyond them came from the desk-member overlay.
                    let overlay_members = members
                        .iter()
                        .filter(|m| !desk.members.contains(m))
                        .cloned()
                        .collect();
                    DeskDto {
                        id: desk.id.clone(),
                        name: desk.name.clone(),
                        description: desk.description.clone(),
                        members,
                        overlay_members,
                        responder: desk.responder,
                        overlay_created: true,
                    }
                });
            manifest_desks.chain(overlay_desks).collect()
        })
        // A company that failed to load surfaces no desks — the console falls
        // back to its static default threads (issue #1757 rework: the Operator
        // feed is its own surface now, fetched through `GET
        // {scope}/operator-channel` rather than injected here).
        .unwrap_or_default();
    Ok(Json(desks))
}

/// The identity of the company's always-present, durable Operator feed
/// (issue #1757 rework). Mirrors `OperatorChannelDto` in
/// `frontend/src/api/types.ts`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OperatorChannelDto {
    /// The channel id — the `desk` query param `GET
    /// {scope}/chat/history?desk=<id>` reads its transcript through.
    id: String,
    /// Always "Operator" — the console's pinned-row label.
    name: String,
    /// The channel's purpose line, shown under the name in the pinned row.
    description: String,
}

/// `GET {scope}/operator-channel` — the identity of the company's dedicated,
/// durable Operator feed: where "what happened and what needs you" workflow
/// reports and the owner/no-mailbox fallback land. A pinned surface, not a
/// desk — the console renders it as its own row below a divider rather than
/// folding it into `GET {scope}/desks`, and it carries no member or mutation
/// routes.
///
/// `id` resolves through
/// [`CompanyRecord::operator_feed_channel`](crate::ports::types::CompanyRecord::operator_feed_channel)
/// — ordinarily [`OPERATOR_CHANNEL`](crate::runtime::OPERATOR_CHANNEL), or
/// [`OPERATOR_CHANNEL_COLLISION_FALLBACK`](crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK)
/// for the one grandfathered company shape where a roster teammate already
/// owns that id — so this and delivery
/// (`workflows::delivery::send_to_channel_adapter`) always agree on where the
/// feed lives. A company with no record yet still gets the default id, so the
/// console always has a channel to point its history read at — but a store
/// read failure is propagated as an error rather than silently answered with
/// the default id: for the grandfathered collision-fallback company, treating
/// a transient failure as "no record" would label the operator's real
/// `operator-feed` transcript as `operator` while delivery keeps targeting the
/// collision-aware address once the store recovers.
async fn operator_channel(
    scope: ScopedCompany,
) -> Result<Json<OperatorChannelDto>, crate::server::Rejection> {
    let id = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .map(|record| record.operator_feed_channel().to_string())
        .unwrap_or_else(|| crate::runtime::OPERATOR_CHANNEL.to_string());
    Ok(Json(OperatorChannelDto {
        id,
        name: "Operator".to_string(),
        description: "Workflow reports and notifications — what happened and what needs you"
            .to_string(),
    }))
}

/// Whether `desk_id` names the built-in `#general` channel rather than a desk
/// (issue #1743; restored PR #1781 review, CodeRabbit P2 — see below).
///
/// `#general` is the company-wide conversation this host has always folded
/// every General spelling into — `general`, `General`, `main`, and the empty
/// string all name it, which is exactly what
/// [`is_general_chat`](crate::server::chat_history::is_general_chat) decides.
/// It is deliberately **not** a desk: it has no lead, no hierarchy, and its
/// membership is the whole roster derived at read time, so there is nothing
/// for a desk mutation to change.
///
/// Guarded on **manifest** desks only, not `desk_exists` (id in manifest *or*
/// overlay) as this predicate's original `da98130c1` shape checked: a company
/// whose blueprint really does declare a `[[group_chat]]` with one of those
/// ids keeps behaving exactly as it did, but an *overlay* desk can only ever
/// hold a reserved id by predating the id/name guards `create_desk` has
/// carried since `da98130c1` and `16dcce235` — the exact grandfathered shape
/// `list_desks` and [`CompanyRecord::resolve_desk_id`] already keep out of the
/// desk list and out of routing (`0c07873db`). Treating it as a real,
/// mutable desk here would contradict that: every other surface already
/// agrees it shadows General, not that it is a desk.
///
/// That read/list-side exclusion (`0c07873db`) is where the gap actually
/// starts: this mutation-side guard (originally `da98130c1`) was dropped by
/// an unrelated refactor (`3cbdb7a5f`) and never restored alongside it — a
/// direct `POST`/`DELETE`/`PUT` to `.../desks/{id}` could still staff,
/// reorder, or delete a desk no read surface exposes, and a write against a
/// bare General spelling with no legacy overlay row regressed from this 409
/// to a misleading 404.
fn is_general_channel(record: &CompanyRecord, desk_id: &str) -> bool {
    crate::server::chat_history::is_general_chat(Some(desk_id))
        && !record.manifest.group_chats.iter().any(|c| c.id == desk_id)
}

/// The path of a desk sub-resource (`desk_id`).
#[derive(Debug, Deserialize)]
struct DeskPath {
    desk_id: String,
}

/// The path of a desk member sub-resource (`desk_id` + `agent_id`).
#[derive(Debug, Deserialize)]
struct DeskMemberPath {
    desk_id: String,
    agent_id: String,
}

/// The add-desk-member body.
#[derive(Debug, Deserialize)]
struct AddDeskMember {
    /// The roster teammate id to add to the desk.
    agent_id: String,
}

/// The set-desk-order body: the operator's explicit member order for a desk.
#[derive(Debug, Deserialize)]
struct SetDeskOrder {
    /// The desk's member ids in the operator's intended order (the hierarchy;
    /// the first is the lead). Every id must be a current effective member of
    /// the desk. An empty list clears the override, resetting to blueprint order.
    ordered_member_ids: Vec<String>,
}

/// `POST {scope}/desks/{desk_id}/members` — add a teammate to a desk through the
/// operator overlay (issue #72). Mirrors the team-overlay write pattern
/// (`ops::team::add_member`): load the record, mutate `overlay_desk_members`,
/// and save. The manifest's `[[group_chat]]` blueprint is never rewritten.
///
/// Validates that the desk exists and that `agent_id` resolves to a roster
/// teammate (a manifest agent or a team-overlay teammate); rejects with
/// `404`/`400` otherwise. Adding a teammate already on the desk (manifest or
/// overlay) is a `409`.
async fn add_desk_member(
    scope: ScopedCompany,
    Path(DeskPath { desk_id }): Path<DeskPath>,
    Json(body): Json<AddDeskMember>,
) -> Result<StatusCode, ApiError> {
    let _guard = scope.runtime.serial.lock().await;
    // Also take `company_write_lock`: this is a load-modify-save cycle over
    // the whole record, exactly the shape every console `ops` writer
    // serializes with that lock. `serial` alone only keeps this out of the
    // way of a live agent cycle — it does nothing against a concurrent
    // `ops` writer (e.g. `patch_company`'s rename), so without this a desk
    // write that loaded the record before the rename landed can save the
    // whole record back afterwards and silently revert it (PR #1875 review
    // finding).
    let write_lock = company_write_lock(scope.id());
    let _write_guard = write_lock.lock().await;
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;
    // The built-in `#general` channel is not a desk and never was — refuse the
    // write with the reason rather than letting it fall through to the
    // desk-not-found answer below (issue #1743; restored PR #1781 review,
    // CodeRabbit P2 — see `is_general_channel`'s own doc).
    if is_general_channel(&record, &desk_id) {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::GENERAL_CHANNEL_IMMUTABLE.to_string(),
        )));
    }
    // The desk must exist — either a manifest blueprint group chat or an
    // operator-created overlay desk (#140). A manifest-only check meant a desk
    // created in the console could be reordered and deleted but never staffed
    // (#833); `desk_exists` is the same check `effective_desk_members` uses.
    if !record.desk_exists(&desk_id) {
        return Err(ApiError(OpenCompanyError::NotFound(format!(
            "desk {desk_id}"
        ))));
    }
    // The agent must resolve to a real teammate (manifest roster or overlay).
    if !record.is_roster_agent(&body.agent_id) {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "no such teammate {}",
            body.agent_id
        ))));
    }
    // A teammate already on the desk (manifest or overlay) is not added twice.
    if record
        .effective_desk_members(&desk_id)
        .iter()
        .any(|m| m == &body.agent_id)
    {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "{} is already on this desk",
            body.agent_id
        ))));
    }
    record.overlay_desk_members.push(OverlayDeskMember {
        desk_id: desk_id.clone(),
        agent_id: body.agent_id.clone(),
    });
    scope.runtime.store().save(&record).await?;
    journal_structural(
        &scope,
        CompanyEvent::DeskMembersChanged {
            desk_id,
            added: vec![body.agent_id],
            removed: Vec::new(),
            by: scope.actor.clone(),
        },
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Append a structural audit row, best-effort.
///
/// Best-effort on purpose, and it is the same posture the episode driver takes
/// with its closing report: the change is already durable on the company record
/// by the time this runs, so a journal that refuses the row must not turn a
/// completed write into a failed request. The row is the audit trail, not the
/// change itself.
async fn journal_structural(scope: &ScopedCompany, event: CompanyEvent) {
    if let Err(err) = scope.runtime.events().append(scope.id(), event).await {
        tracing::warn!(error = %err, "structural audit row could not be journaled");
    }
}

/// One desk's move grammar, as the console renders and edits it.
///
/// Two shapes in one payload, and the split is the point:
///
/// - `declared` is the block **as authored** — every field an `Option` whose
///   `None` means "not said". It is what a `PUT` round-trips.
/// - `effective` is what the runtime will actually use, with every default
///   resolved against the current membership.
///
/// One number could not carry both. `turn_budget = 9` on a three-seat desk is
/// either an operator's decision or the derived `3 x members`, and the two
/// behave differently the moment somebody joins — so a console showing a single
/// figure cannot say whether adding a seat will change it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeskHiveDto {
    desk_id: String,
    /// `"overlay"` when an operator installed this, `"manifest"` when the
    /// blueprint declares it, `"default"` when neither does.
    source: &'static str,
    /// Whether an episode would actually open right now.
    ///
    /// A one-member desk with `enabled = true` is still `false`: the flag says
    /// what the operator wants, not what the desk is able to do, and a console
    /// that showed "deliberates" for a desk of one would be describing a room
    /// that cannot exist.
    deliberates: bool,
    /// The block as authored. Snake_case, deliberately: this field **is** the
    /// manifest block, the console's editor edits it directly, and a camelCase
    /// twin would be a second shape to keep in step with the TOML.
    declared: crate::hivemind::HiveConfig,
    effective: EffectiveHiveDto,
    /// Every move a table may name, and the three no table can take away.
    move_kinds: &'static [&'static str],
    ungated_kinds: &'static [&'static str],
    seats: Vec<HiveSeatDto>,
    /// How many seats may deposit a distinct supporter, and whether that clears
    /// the effective quorum.
    ///
    /// `!propose` counts here, because in the fold a proposal is already its own
    /// author's support. When `reaches_quorum` is false the desk answers with a
    /// single responder however much the room agrees — `desk_episode` declines —
    /// and the console has to be able to say so.
    eligible_supporters: u32,
    reaches_quorum: bool,
}

/// What the runtime will use, with every default resolved.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EffectiveHiveDto {
    turn_budget: u32,
    quorum: u32,
    blind_round: bool,
    dominance_cap: u32,
    repetition_cap: u32,
    require_grounded: bool,
    require_evidential: bool,
    refutation_cap: Option<u32>,
}

/// One seat, and the moves it may open a line with.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HiveSeatDto {
    agent_id: String,
    label: String,
    role: String,
    /// Already in `MOVE_KINDS` order and already unioned with the ungated three,
    /// so the console renders this verbatim rather than re-deriving it.
    moves: Vec<&'static str>,
    /// Whether the table governs this seat at all.
    ///
    /// Invisible from `moves` alone — "named with every kind" and "not named"
    /// produce the same list, and only one of them is a decision somebody made.
    governed: bool,
}

/// Build the payload for one desk.
fn desk_hive_dto(record: &crate::ports::CompanyRecord, desk_id: &str) -> DeskHiveDto {
    let config = record.effective_desk_hive(desk_id);
    let members: Vec<String> = record
        .effective_desk_members(desk_id)
        .into_iter()
        .filter(|id| record.is_roster_agent(id))
        .collect();
    let policy = crate::hivemind::HivePolicy::from_config(&config, members.len()).episode;
    let eligible = members
        .iter()
        .filter(|id| config.may(id, "support") || config.may(id, "propose"))
        .count();
    let seats = members
        .iter()
        .map(|id| {
            let agent = record.effective_agents().into_iter().find(|a| &a.id == id);
            HiveSeatDto {
                agent_id: id.clone(),
                label: agent
                    .as_ref()
                    .and_then(|a| a.name.clone())
                    .unwrap_or_else(|| id.clone()),
                role: agent.map(|a| a.role.clone()).unwrap_or_default(),
                moves: config.moves_for(id),
                governed: config.moves.get(id).is_some_and(|kinds| !kinds.is_empty()),
            }
        })
        .collect();
    let source = if record.desk_hive_is_installed(desk_id) {
        "overlay"
    } else if record
        .manifest
        .group_chats
        .iter()
        .any(|group| group.id == desk_id)
    {
        "manifest"
    } else {
        "default"
    };
    DeskHiveDto {
        desk_id: desk_id.to_string(),
        source,
        deliberates: config.deliberates(members.len()),
        effective: EffectiveHiveDto {
            turn_budget: policy.turn_budget,
            quorum: policy.quorum.threshold,
            blind_round: policy.blind_round,
            dominance_cap: policy.dominance_cap,
            repetition_cap: policy.repetition_cap,
            require_grounded: policy.quorum.require_grounded,
            require_evidential: policy.quorum.require_evidential,
            refutation_cap: policy.quorum.refutation_cap,
        },
        declared: config,
        move_kinds: crate::hivemind::MOVE_KINDS,
        ungated_kinds: crate::hivemind::UNGATED_KINDS,
        eligible_supporters: u32::try_from(eligible).unwrap_or(u32::MAX),
        reaches_quorum: u32::try_from(eligible)
            .is_ok_and(|eligible| eligible >= policy.quorum.threshold),
        seats,
    }
}

/// `GET {scope}/desks/{desk_id}/hive` — the move grammar in force on a desk.
async fn desk_hive(
    scope: ScopedCompany,
    Path(DeskPath { desk_id }): Path<DeskPath>,
) -> Result<Json<DeskHiveDto>, ApiError> {
    let record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;
    if !record.desk_exists(&desk_id) {
        return Err(ApiError(OpenCompanyError::NotFound(format!(
            "desk {desk_id}"
        ))));
    }
    Ok(Json(desk_hive_dto(&record, &desk_id)))
}

/// `PUT {scope}/desks/{desk_id}/hive` — install or replace a desk's move
/// grammar, without rewriting the version-controlled `[[group_chat]]` block.
///
/// The body is a bare `HiveConfig` — the manifest block verbatim. Unknown fields
/// are ignored rather than refused, so a newer console against an older server
/// degrades instead of 400-ing.
///
/// **Validated against the desk's *effective* roster**, not its declared one, so
/// overlay additions and Team-API retirements are what the table is judged by.
/// That can legitimately refuse a config the manifest would have accepted — a
/// table valid when `company.toml` was written stops being valid once a seat
/// retires — which is why the message names the desk.
///
/// An episode already running is unaffected: `EpisodeDriver` holds its
/// `HiveDesk` as a snapshot for the episode's life, so a room cannot have its
/// quorum moved underneath it mid-argument. The change takes effect on the next
/// message that opens one.
async fn set_desk_hive(
    scope: ScopedCompany,
    Path(DeskPath { desk_id }): Path<DeskPath>,
    Json(body): Json<crate::hivemind::HiveConfig>,
) -> Result<Json<DeskHiveDto>, ApiError> {
    let _guard = scope.runtime.serial.lock().await;
    // The same load-modify-save serialization every desk write takes; see
    // `add_desk_member` for why `serial` alone is not enough.
    let write_lock = company_write_lock(scope.id());
    let _write_guard = write_lock.lock().await;
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;
    if is_general_channel(&record, &desk_id) {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::GENERAL_CHANNEL_IMMUTABLE.to_string(),
        )));
    }
    if !record.desk_exists(&desk_id) {
        return Err(ApiError(OpenCompanyError::NotFound(format!(
            "desk {desk_id}"
        ))));
    }
    let members: Vec<String> = record
        .effective_desk_members(&desk_id)
        .into_iter()
        .filter(|id| record.is_roster_agent(id))
        .collect();
    // The manifest's own checks, on the manifest's own words — one
    // implementation, so the runtime cannot accept what a `company.toml` with
    // the same block would be refused for.
    let problems = crate::company::hive_problems(&format!("desk `{desk_id}`"), &members, &body);
    if !problems.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            problems.join(" "),
        )));
    }
    record.upsert_desk_hive(crate::ports::types::DeskHiveOverride {
        desk_id: desk_id.clone(),
        hive: body,
    });
    scope.runtime.store().save(&record).await?;
    journal_structural(
        &scope,
        CompanyEvent::DeskHiveConfigured {
            desk_id: desk_id.clone(),
            reset: false,
            by: scope.actor.clone(),
        },
    )
    .await;
    // The derived result of what was just installed, so the console renders the
    // effective numbers without a second round trip.
    Ok(Json(desk_hive_dto(&record, &desk_id)))
}

/// `DELETE {scope}/desks/{desk_id}/hive` — drop the installed grammar and fall
/// back to whatever the manifest declares.
///
/// Because the override is a sibling collection rather than a merged field,
/// "restore the blueprint's version" is a `retain` and nothing else.
async fn reset_desk_hive(
    scope: ScopedCompany,
    Path(DeskPath { desk_id }): Path<DeskPath>,
) -> Result<Json<DeskHiveDto>, ApiError> {
    let _guard = scope.runtime.serial.lock().await;
    let write_lock = company_write_lock(scope.id());
    let _write_guard = write_lock.lock().await;
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;
    if !record.desk_exists(&desk_id) {
        return Err(ApiError(OpenCompanyError::NotFound(format!(
            "desk {desk_id}"
        ))));
    }
    // A reset with nothing installed is not an error: the caller asked for the
    // manifest's grammar and the manifest's grammar is what they now have.
    if record.clear_desk_hive(&desk_id) {
        scope.runtime.store().save(&record).await?;
        journal_structural(
            &scope,
            CompanyEvent::DeskHiveConfigured {
                desk_id: desk_id.clone(),
                reset: true,
                by: scope.actor.clone(),
            },
        )
        .await;
    }
    Ok(Json(desk_hive_dto(&record, &desk_id)))
}

/// `PUT {scope}/desks/{desk_id}/order` — set the operator's explicit member
/// order (the desk hierarchy) for a desk through the overlay (issue #131). The
/// version-controlled `[[group_chat]]` blueprint is never rewritten; the order
/// lives entirely in the [`OverlayDeskOrder`] overlay and is applied at read
/// time by [`CompanyRecord::effective_desk_members`].
///
/// Validates that the desk exists in the manifest (`404`), that the body has no
/// duplicate ids (`400`), and that every id is a current effective member of the
/// desk (`400`, naming the offending id). An empty `ordered_member_ids` clears
/// the desk's order override, resetting it to the blueprint order.
async fn set_desk_order(
    scope: ScopedCompany,
    Path(DeskPath { desk_id }): Path<DeskPath>,
    Json(body): Json<SetDeskOrder>,
) -> Result<StatusCode, ApiError> {
    let _guard = scope.runtime.serial.lock().await;
    // Also take `company_write_lock`: this is a load-modify-save cycle over
    // the whole record, exactly the shape every console `ops` writer
    // serializes with that lock. `serial` alone only keeps this out of the
    // way of a live agent cycle — it does nothing against a concurrent
    // `ops` writer (e.g. `patch_company`'s rename), so without this a desk
    // write that loaded the record before the rename landed can save the
    // whole record back afterwards and silently revert it (PR #1875 review
    // finding).
    let write_lock = company_write_lock(scope.id());
    let _write_guard = write_lock.lock().await;
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;
    // The built-in `#general` channel is not a desk and never was — refuse the
    // write with the reason rather than letting it fall through to the
    // desk-not-found answer below (issue #1743; restored PR #1781 review,
    // CodeRabbit P2 — see `is_general_channel`'s own doc).
    if is_general_channel(&record, &desk_id) {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::GENERAL_CHANNEL_IMMUTABLE.to_string(),
        )));
    }
    // The desk must exist — either a manifest blueprint group chat or an
    // operator-created overlay desk (#140). `desk_exists` covers both (the same
    // check `effective_desk_members` uses), so an operator-created desk can be
    // reordered / have its lead changed too, not just manifest desks.
    if !record.desk_exists(&desk_id) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "desk {desk_id}"
        ))));
    }
    // Reject duplicate ids in the requested order.
    for (i, id) in body.ordered_member_ids.iter().enumerate() {
        if body.ordered_member_ids[..i].contains(id) {
            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                "duplicate member {id} in desk order"
            ))));
        }
    }
    // Every id must be a current effective member of the desk.
    let members = record.effective_desk_members(&desk_id);
    if let Some(unknown) = body
        .ordered_member_ids
        .iter()
        .find(|id| !members.contains(id))
    {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "{unknown} is not a member of this desk"
        ))));
    }
    // Replace-or-insert this desk's order override. An empty list removes it,
    // resetting the desk to its blueprint order.
    record.overlay_desk_order.retain(|o| o.desk_id != desk_id);
    if !body.ordered_member_ids.is_empty() {
        record.overlay_desk_order.push(OverlayDeskOrder {
            desk_id,
            ordered: body.ordered_member_ids,
        });
    }
    scope.runtime.store().save(&record).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE {scope}/desks/{desk_id}/members/{agent_id}` — remove an
/// operator-added desk member (issue #72). A manifest-declared member is part of
/// the blueprint and cannot be removed here (`409`); an id that is not an
/// overlay member of the desk is a `404`.
async fn remove_desk_member(
    scope: ScopedCompany,
    Path(DeskMemberPath { desk_id, agent_id }): Path<DeskMemberPath>,
) -> Result<StatusCode, ApiError> {
    let _guard = scope.runtime.serial.lock().await;
    // Also take `company_write_lock`: this is a load-modify-save cycle over
    // the whole record, exactly the shape every console `ops` writer
    // serializes with that lock. `serial` alone only keeps this out of the
    // way of a live agent cycle — it does nothing against a concurrent
    // `ops` writer (e.g. `patch_company`'s rename), so without this a desk
    // write that loaded the record before the rename landed can save the
    // whole record back afterwards and silently revert it (PR #1875 review
    // finding).
    let write_lock = company_write_lock(scope.id());
    let _write_guard = write_lock.lock().await;
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;
    // The built-in `#general` channel is not a desk and never was — refuse the
    // write with the reason rather than letting it fall through to the
    // desk-not-found answer below (issue #1743; restored PR #1781 review,
    // CodeRabbit P2 — see `is_general_channel`'s own doc).
    if is_general_channel(&record, &desk_id) {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::GENERAL_CHANNEL_IMMUTABLE.to_string(),
        )));
    }
    // First validate that the desk exists at all — otherwise a caller supplying
    // an unknown desk_id gets a desk-scoped 404 rather than a confusing
    // member-scoped one (Greptile feedback). Existence spans both blueprint and
    // operator-created overlay desks (#140); a manifest-only check here stranded
    // console-created desks with members that could never be removed (#833).
    if !record.desk_exists(&desk_id) {
        return Err(ApiError(OpenCompanyError::NotFound(format!(
            "desk {desk_id}"
        ))));
    }
    // A manifest desk member belongs to the version-controlled blueprint.
    let is_manifest_member = record
        .manifest
        .group_chats
        .iter()
        .find(|c| c.id == desk_id)
        .is_some_and(|c| c.members.iter().any(|m| m == &agent_id));
    if is_manifest_member {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::MANIFEST_DESK_MEMBER_DELETE.to_string(),
        )));
    }
    let before = record.overlay_desk_members.len();
    record
        .overlay_desk_members
        .retain(|m| !(m.desk_id == desk_id && m.agent_id == agent_id));
    if record.overlay_desk_members.len() == before {
        return Err(ApiError(OpenCompanyError::NotFound(format!(
            "desk member {agent_id}"
        ))));
    }
    // Keep the desk-order overlay consistent: drop the removed id from this
    // desk's hierarchy, and drop the whole entry if it empties (issue #131).
    for order in record
        .overlay_desk_order
        .iter_mut()
        .filter(|o| o.desk_id == desk_id)
    {
        order.ordered.retain(|id| id != &agent_id);
    }
    record
        .overlay_desk_order
        .retain(|o| !(o.desk_id == desk_id && o.ordered.is_empty()));
    scope.runtime.store().save(&record).await?;
    journal_structural(
        &scope,
        CompanyEvent::DeskMembersChanged {
            desk_id,
            added: Vec::new(),
            removed: vec![agent_id],
            by: scope.actor.clone(),
        },
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// The create-desk body. `name` is required; `id` is optional (derived from the
/// name when omitted); `description` and `members` are optional.
#[derive(Debug, Deserialize)]
struct CreateDesk {
    /// The desk's display name (required).
    name: String,
    /// An optional description of what the desk is for.
    #[serde(default)]
    description: Option<String>,
    /// An optional explicit desk id (snake_case). Derived from `name` when
    /// omitted.
    #[serde(default)]
    id: Option<String>,
    /// The desk's founding member ids, in order (the first becomes the lead —
    /// unless `responder` is `"auto"`, in which case order carries no rank).
    /// Each must resolve to a roster teammate. Optional — a desk can start empty
    /// and gain members through the desk-member overlay.
    #[serde(default)]
    members: Vec<String>,
    /// How the desk routes its unmentioned messages (issue #1835). Absent means
    /// `"lead"` — today's model, and what every existing caller sends — so the
    /// org chart's create is unchanged. `"auto"` creates a leadless channel
    /// whose answerer is picked per message.
    #[serde(default)]
    responder: ResponderMode,
}

/// Derives a snake_case desk id from a display name: lowercase, runs of
/// non-alphanumeric characters collapse to a single `_`, leading/trailing `_`
/// trimmed. Returns an empty string when the name has no alphanumerics (the
/// caller then rejects it as an invalid id).
fn slugify_desk_id(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_us = true; // trims leading underscores
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// Whether `id` is a valid desk id: non-empty and only ascii lowercase letters,
/// digits, or underscores. Mirrors the manifest's `[[group_chat]]` id rule so a
/// runtime-created desk id is indistinguishable from a blueprint one.
fn is_valid_desk_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `POST {scope}/desks` — create a desk through the operator overlay. Mirrors the
/// desk-member write pattern (`add_desk_member`): load the record, mutate an
/// overlay collection, and save. The manifest's `[[group_chat]]` blueprint is
/// never rewritten, and the created desk is preserved across rebuilds like every
/// other overlay.
///
/// Validates that `name` is non-empty, the (given or derived) id is snake_case
/// and not already taken by a manifest or overlay desk (`400`/`409`), and every
/// member resolves to a roster teammate (`400`). Returns the created desk.
async fn create_desk(
    scope: ScopedCompany,
    Json(body): Json<CreateDesk>,
) -> Result<(StatusCode, Json<DeskDto>), ApiError> {
    let _guard = scope.runtime.serial.lock().await;
    // Also take `company_write_lock`: this is a load-modify-save cycle over
    // the whole record, exactly the shape every console `ops` writer
    // serializes with that lock. `serial` alone only keeps this out of the
    // way of a live agent cycle — it does nothing against a concurrent
    // `ops` writer (e.g. `patch_company`'s rename), so without this a desk
    // write that loaded the record before the rename landed can save the
    // whole record back afterwards and silently revert it (PR #1875 review
    // finding).
    let write_lock = company_write_lock(scope.id());
    let _write_guard = write_lock.lock().await;
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;

    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "desk name is required".to_string(),
        )));
    }
    // An explicit id is honored (trimmed); otherwise derive one from the name.
    let id = match body.id.as_deref().map(str::trim) {
        Some(explicit) if !explicit.is_empty() => explicit.to_string(),
        _ => slugify_desk_id(&name),
    };
    if !is_valid_desk_id(&id) {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "invalid desk id {id:?} — use lowercase letters, digits, and underscores"
        ))));
    }
    // A desk that claimed one of the General spellings would shadow the
    // built-in `#general` channel: the console would show two `#general` rows
    // and the host would route messages addressed to it at this desk's lead
    // instead of the orchestrator (issue #1743). Refused at creation, which
    // costs nothing — no manifest can reach this path, so no existing company
    // loses a desk.
    //
    // The **display name** is reserved for the same reason and not a weaker
    // one: `resolve_desk_id` matches a desk by id *or* by case-insensitive
    // name, so `{"id": "ops", "name": "General"}` shadows the channel just as
    // thoroughly — `everyone_desk` folds the built-in `main` thread to
    // `General`, that lookup then selects this desk, and `@everyone` on the
    // company-wide line expands to its members instead of the roster.
    if crate::server::chat_history::is_general_chat(Some(&id))
        || crate::server::chat_history::is_general_chat(Some(&name))
    {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::GENERAL_CHANNEL_RESERVED.to_string(),
        )));
    }
    // Issue #1757: `operator` is reserved for the built-in, read-only Operator
    // system channel — `desk_exists` alone would miss this, since the system
    // channel is never a manifest or overlay desk. Without this, a created
    // overlay desk with this id would collide with the system channel in the
    // desk list, and every message to it would be refused by the read-only
    // guard in `chat_and_emit`, which treats any `chat_id == OPERATOR_CHANNEL`
    // as the system feed regardless of where it came from.
    //
    // The **display name** is reserved for the same reason the General
    // display name is, above, and not a weaker one (PR #1781 review,
    // CodeRabbit P2 follow-up to `316bc9229`): `CompanyRecord::resolve_desk_id`
    // matches an overlay desk by id *or* case-insensitive name, so
    // `{"id": "ops", "name": "Operator"}` resolves a `?desk=Operator` selector
    // to this desk exactly as thoroughly as claiming the literal id would.
    // Refused at creation like the General case above, for the same reason:
    // no manifest can reach this API path, so no existing company loses a
    // desk — a *newly created* overlay desk can never reach the shape below.
    //
    // A **manifest** desk grandfathered onto this name from before
    // `316bc9229` — the case this creation guard cannot cover, since it
    // already existed — used to hit exactly the mismatch this paragraph
    // warned about: `ensure_desk_writable` (`company/runtime.rs`) checked the
    // *raw* selector string against `OPERATOR_CHANNEL` before any resolution
    // ran, so a write addressed to the desk's `Operator` alias was refused as
    // the read-only system feed while a write addressed to its real id sailed
    // straight through. Fixed (issue #1781 review, Codex P1 follow-up):
    // `ensure_desk_writable` now resolves the raw selector through
    // `resolve_desk_id` first, so it agrees with the read path on which desk
    // a caller meant. The fallback address
    // (`OPERATOR_CHANNEL_COLLISION_FALLBACK`, "operator-feed")
    // is reserved by name for the identical reason `316bc9229` reserved it on
    // the manifest side — `resolve_desk` folds a `?desk=` selector against it
    // the same way — but not by id: `is_valid_desk_id` above already rejects
    // any hyphen, so no `id` can ever equal the hyphenated fallback constant.
    if id == crate::runtime::OPERATOR_CHANNEL
        || name.eq_ignore_ascii_case(crate::runtime::OPERATOR_CHANNEL)
    {
        return Err(ApiError(OpenCompanyError::Conflict(
            "the id \"operator\" is reserved for the built-in Operator channel — choose a different id"
                .to_string(),
        )));
    }
    if name.eq_ignore_ascii_case(crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK) {
        return Err(ApiError(OpenCompanyError::Conflict(
            "the name \"operator-feed\" is reserved for the built-in Operator channel's \
             fallback feed — choose a different name"
                .to_string(),
        )));
    }
    if record.desk_exists(&id) {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "a desk with id {id:?} already exists"
        ))));
    }
    // Validate + dedup the founding members; each must be a roster teammate.
    let mut members: Vec<String> = Vec::with_capacity(body.members.len());
    for member in body.members {
        if !record.is_roster_agent(&member) {
            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                "no such teammate {member}"
            ))));
        }
        if !members.contains(&member) {
            members.push(member);
        }
    }

    // An `auto` channel with nobody in it is unroutable by construction
    // (issue #1835, codex review): the selector has no candidates and the
    // first-member fallback has no first member, so an unmentioned message
    // there would fall through to the orchestrator — contradicting the
    // channel's own stated model. A *lead* desk may still start empty and be
    // staffed from the org chart, exactly as before.
    if !body.responder.is_lead() && members.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "a channel with per-message routing needs at least one member — with nobody in it, there is nobody to pick"
                .to_string(),
        )));
    }
    let description = body
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    let desk = OverlayDesk {
        id: id.clone(),
        name: name.clone(),
        description: description.clone(),
        members: members.clone(),
        responder: body.responder,
        // A desk created here starts with no hive block of its own and takes
        // the defaults, exactly as a manifest desk that declares none does.
        hive: crate::hivemind::HiveConfig::default(),
    };
    record.overlay_desks.push(desk);
    scope.runtime.store().save(&record).await?;
    journal_structural(
        &scope,
        CompanyEvent::DeskCreated {
            desk_id: id.clone(),
            name: name.clone(),
            members: members.clone(),
            by: scope.actor.clone(),
        },
    )
    .await;

    let effective = record.effective_desk_members(&id);
    Ok((
        StatusCode::CREATED,
        Json(DeskDto {
            id,
            name,
            description,
            members: effective,
            overlay_members: Vec::new(),
            responder: body.responder,
            overlay_created: true,
        }),
    ))
}

/// `DELETE {scope}/desks/{desk_id}` — delete an operator-created desk. A
/// manifest-declared desk is part of the version-controlled blueprint and cannot
/// be deleted here (`409`); an unknown desk id is a `404`. Deleting an overlay
/// desk also drops any desk-member overlay rows that targeted it, so no orphan
/// membership survives.
async fn delete_desk(
    scope: ScopedCompany,
    Path(DeskPath { desk_id }): Path<DeskPath>,
) -> Result<StatusCode, ApiError> {
    let _guard = scope.runtime.serial.lock().await;
    // Also take `company_write_lock`: this is a load-modify-save cycle over
    // the whole record, exactly the shape every console `ops` writer
    // serializes with that lock. `serial` alone only keeps this out of the
    // way of a live agent cycle — it does nothing against a concurrent
    // `ops` writer (e.g. `patch_company`'s rename), so without this a desk
    // write that loaded the record before the rename landed can save the
    // whole record back afterwards and silently revert it (PR #1875 review
    // finding).
    let write_lock = company_write_lock(scope.id());
    let _write_guard = write_lock.lock().await;
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;

    // The built-in `#general` channel is not a desk and never was — refuse the
    // write with the reason rather than letting it fall through to the
    // desk-not-found answer below (issue #1743; restored PR #1781 review,
    // CodeRabbit P2 — see `is_general_channel`'s own doc).
    if is_general_channel(&record, &desk_id) {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::GENERAL_CHANNEL_IMMUTABLE.to_string(),
        )));
    }
    // A manifest desk belongs to the blueprint — never deletable at runtime.
    if record.manifest.group_chats.iter().any(|c| c.id == desk_id) {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::MANIFEST_DESK_DELETE.to_string(),
        )));
    }
    // Tombstone the operator-feed divert before it can be lost (issue #1781
    // review, Codex P2): `operator_feed_channel` currently diverts only while
    // *something* live holds the id or display name `operator`, and the desk
    // this call is about to remove may be that something. Recorded here,
    // before the removal, while the live check can still see it — see
    // `CompanyRecord::divert_operator_feed_permanently`'s doc for why this
    // has to survive the desk being gone.
    if record.operator_feed_channel()
        == crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK
    {
        record.divert_operator_feed_permanently();
    }
    let before = record.overlay_desks.len();
    record.overlay_desks.retain(|d| d.id != desk_id);
    if record.overlay_desks.len() == before {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "desk {desk_id}"
        ))));
    }
    // Drop any member-overlay rows that targeted the now-deleted desk.
    record.overlay_desk_members.retain(|m| m.desk_id != desk_id);
    // And the installed move grammar, for the same reason. Left behind, an
    // overlay desk re-created with the same id silently inherits a grammar
    // nobody installed on it — a desk deliberating under a table its operator
    // never wrote, which is the drift the overlay layer exists to prevent.
    record.clear_desk_hive(&desk_id);
    scope.runtime.store().save(&record).await?;
    journal_structural(
        &scope,
        CompanyEvent::DeskDeleted {
            desk_id,
            by: scope.actor.clone(),
        },
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Logs SSE stream teardown when the subscriber disconnects. Held inside the
/// projection closure so it drops exactly when the response body is dropped.
///
/// Also owns the label-refresh task's handle, so the periodic roster re-read
/// dies with its connection instead of leaking for the process's lifetime.
struct SseStreamGuard {
    company: CompanyId,
    /// One-shot stop signal for the label-refresh task. Sent before the handle
    /// is aborted so the loop exits at its next sleep boundary rather than
    /// waking once more to write a roster map nobody will read.
    cancel: Option<oneshot::Sender<()>>,
    label_refresh: Option<JoinHandle<()>>,
}

impl Drop for SseStreamGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(handle) = self.label_refresh.take() {
            handle.abort();
        }
        tracing::debug!(company = %self.company, "operator SSE stream closed");
    }
}

/// How often an open SSE stream re-reads the roster, so a mention chip for a
/// user added or renamed after the stream opened picks up the new label.
const LABEL_REFRESH_EVERY: Duration = Duration::from_secs(60);

/// Re-derives whether `actor` (the human behind an open SSE connection) still
/// holds admin access, for [`company_events`]'s periodic refresh AND its
/// per-item revalidation of an owner-fallback report.
///
/// Fixes issue #1781 review (Codex P1): the `is_admin` this feeds used to be
/// captured once at stream-open time and never reconsidered, so a mid-stream
/// demotion kept projecting the admin-only owner-fallback report to the
/// now-non-admin user for as long as their tab stayed open — `PATCH
/// …/users/{id}` updates the stored role without revoking sessions on a plain
/// demotion (only a suspension does that; see `src/server/users/admin.rs`'s
/// `update_user`), and an already-open SSE response performs no further
/// authentication of its own.
///
/// Returns `previous` unchanged only for the machine principal (`actor:
/// None`, unrestricted by construction per [`ScopedCompany::is_admin`]'s own
/// doc) — every other outcome (`Ok(None)`, the user record has gone missing,
/// or `Err`, the store read itself failed) returns `false` (issue #1781
/// review, Codex P1 follow-up to this fix). Fail-open on a lookup failure was
/// the original shape, on the reasoning that "a transient read failure
/// should not flip a live connection's access either way" — true for the
/// periodic refresh alone, which only ever *feeds* a decision, but
/// [`is_admin_for_item`] also calls this synchronously, per item, as the
/// actual gate on the one admin-only content class this whole mechanism
/// exists to protect. There, `previous` is exactly the stale cached value a
/// demotion may have already invalidated — failing open on top of a store
/// hiccup would hand a demoted, now-unconfirmable actor the benefit of the
/// doubt on the read that was supposed to catch the demotion. A human
/// principal whose current role cannot be confirmed is treated as not admin;
/// only the always-safe machine principal keeps its unconditional pass.
async fn refreshed_is_admin(
    runtime: &CompanyRuntime,
    actor: Option<&Actor>,
    previous: bool,
) -> bool {
    let Some(actor) = actor else {
        return previous;
    };
    match runtime.users().get_user(runtime.id(), &actor.id).await {
        Ok(Some(user)) => {
            user.role.may_administer() && user.status == crate::ports::users::UserStatus::Active
        }
        _ => false,
    }
}

/// `GET {scope}/events` — the company → operator attention feed (issue #66).
///
/// Subscribes to the company's [`EventLog`](crate::ports::EventLog) and streams a
/// **safe projection** of each attention-worthy [`CompanyEvent`] to the console
/// as Server-Sent Events. Only domain fields already present on the event reach
/// the wire — never a token, secret, credential, or raw webhook/tool payload —
/// and events that carry no attention signal (or that carry raw internal state)
/// are dropped entirely (see [`project_event`]). Auth rides the same
/// [`ScopedCompany`] guard as every other company-scoped route: the browser's
/// `EventSource` sends the session cookie same-origin, so no new auth path is
/// introduced.
async fn company_events(
    scope: ScopedCompany,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let company = scope.id().clone();
    tracing::debug!(company = %company, "operator SSE stream opening");
    let viewer = scope
        .actor
        .as_ref()
        .map(|actor| Viewer::User(actor.id.clone()))
        .unwrap_or(Viewer::Operator);
    // Threaded into the projection below so a live `AgentReply` from the
    // owner-fallback pseudo-author is gated the same way a reload's
    // `history_for_desk` already gates it (issue #1781 review, Codex P1) — a
    // non-admin must never see the admin-only report just because they had
    // the stream open when it landed.
    //
    // Held in a shared cell, not a captured `bool`: `scope.is_admin` is only
    // this connection's role *at open time*, and this stream can outlive a
    // demotion. `PATCH …/users/{id}` updates the stored role without
    // revoking sessions on a plain demotion (only a suspension does that),
    // and an already-open SSE response performs no further authentication —
    // so a captured `true` would keep projecting the owner-fallback report to
    // a now-non-admin user for as long as their tab stayed open (issue #1781
    // review, Codex P1). The periodic refresh below re-derives it from the
    // live user record, the same bounded staleness window the label refresh
    // just below already accepts for mention chips.
    let is_admin = Arc::new(std::sync::atomic::AtomicBool::new(scope.is_admin));
    let subscription = scope.runtime.events().subscribe(&company);
    // Roster display labels for mention chips. Held in a shared lock rather
    // than captured once: the stream outlives membership changes that can add
    // or rename a user, and a transiently failed initial read must not fix the
    // map empty for the rest of the connection. A background task refreshes it
    // on an interval, and the guard above aborts that task when the stream
    // closes.
    let authors: Arc<std::sync::RwLock<std::collections::HashMap<String, String>>> = Arc::new(
        std::sync::RwLock::new(author_labels(&scope.runtime).await.unwrap_or_default()),
    );
    let (cancel, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let label_refresh = {
        let runtime = scope.runtime.clone();
        let shared = Arc::clone(&authors);
        let is_admin_cell = Arc::clone(&is_admin);
        let actor = scope.actor.clone();
        tokio::spawn(async move {
            let mut cancel = cancel_rx;
            loop {
                // The guard's one-shot fires when the stream closes, so the
                // loop stops at the next boundary instead of waking once more
                // to attempt a write nobody will read.
                tokio::select! {
                    _ = tokio::time::sleep(LABEL_REFRESH_EVERY) => {}
                    _ = &mut cancel => return,
                }
                if let Ok(fresh) = author_labels(&runtime).await {
                    *shared
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = fresh;
                }
                let previous = is_admin_cell.load(std::sync::atomic::Ordering::Relaxed);
                let refreshed = refreshed_is_admin(&runtime, actor.as_ref(), previous).await;
                is_admin_cell.store(refreshed, std::sync::atomic::Ordering::Relaxed);
            }
        })
    };
    let guard = SseStreamGuard {
        company: company.clone(),
        cancel: Some(cancel),
        label_refresh: Some(label_refresh),
    };
    // A second handle on the same runtime/actor the label-refresh task above
    // captured its own clones of — needed here too, for the per-item
    // revalidation below (issue #1781 review, Codex P1 follow-up).
    let runtime = scope.runtime.clone();
    let actor = scope.actor.clone();
    let durable = subscription.filter_map(move |item| {
        // Keep the teardown guard alive for the life of the stream.
        let _ = &guard;
        let authors = Arc::clone(&authors);
        let is_admin_cell = Arc::clone(&is_admin);
        let runtime = runtime.clone();
        let actor = actor.clone();
        let viewer = viewer.clone();
        async move {
            let cached = is_admin_cell.load(std::sync::atomic::Ordering::Relaxed);
            let is_admin = is_admin_for_item(&item, &runtime, actor.as_ref(), cached).await;
            let authors = authors
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            project_stream_item_for_viewer(&item, &authors, &viewer, is_admin)
                .map(|value| Ok(Event::default().data(value.to_string())))
        }
    });
    // Merge the transient live turn-progress bus (tool_call/tool_result frames a
    // turn emits while it runs — see [`crate::turn_stream`]) onto the same feed.
    // These are ephemeral and never journaled; the console switches on `type`
    // just like the durable projections. On a company with no active turn this
    // stream is simply quiet.
    //
    // A typing frame authored by this very connection is dropped here rather
    // than by the console: the bus fans a ping out to every subscriber of the
    // company, including its sender, so without this a composer would echo its
    // own "You are typing…" line back at itself for the length of the ping's
    // TTL. Presence is left alone — a console does not render its own dot from
    // the live feed, so there is nothing to echo.
    let self_id = scope.actor.as_ref().map(|a| a.id.clone());
    let live = crate::turn_stream::subscribe(&company)
        .filter_map(move |frame| {
            let drop = is_own_typing_frame(&frame, self_id.as_deref());
            std::future::ready(if drop { None } else { Some(frame) })
        })
        .map(|frame| {
            Ok::<Event, Infallible>(
                Event::default().data(serde_json::to_string(&frame).unwrap_or_default()),
            )
        });
    let stream = futures::stream::select(durable, live);
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

/// Whether a live frame is a typing ping authored by the very connection about
/// to receive it.
///
/// The typing bus fans one ping out to every subscriber in the company,
/// including its sender — there is no per-listener addressing beneath it — so
/// without this check a console's own composer would echo its own "You are
/// typing…" line back at itself for the length of the ping's TTL. Presence
/// frames are left alone: a console never renders its own dot from the live
/// feed, so there is nothing there to echo.
fn is_own_typing_frame(frame: &crate::turn_stream::LiveFrame, self_id: Option<&str>) -> bool {
    matches!(
        frame,
        crate::turn_stream::LiveFrame::Typing(typing)
            if self_id == Some(typing.user_id.as_str())
    )
}

/// Whether `item` is the one content class [`company_events`]'s `is_admin`
/// gates: an owner-fallback [`AgentReply`](CompanyEvent::AgentReply) —
/// journaled under
/// [`OWNER_FALLBACK_REPORT_AUTHOR`](crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR)
/// (issue #1781 review, Codex P1 follow-up).
///
/// A cheap, synchronous pre-check so `company_events`'s per-item revalidation
/// only spends a store read on the one content class that needs fresher-than-
/// `LABEL_REFRESH_EVERY` staleness — every other event (and a stream `Gap`)
/// keeps using the cached snapshot with no store read added to its path.
fn is_owner_fallback_report(item: &EventStreamItem) -> bool {
    matches!(
        item,
        EventStreamItem::Event(StoredEvent {
            event: CompanyEvent::AgentReply { agent_id, .. },
            ..
        }) if agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR
    )
}

/// The `is_admin` value [`company_events`] projects `item` under (issue #1781
/// review, Codex P1 follow-up).
///
/// `cached` is the periodic `LABEL_REFRESH_EVERY`-bounded snapshot every other
/// event uses unchanged. An owner-fallback report is revalidated fresh
/// instead — the P1 finding's fix: without this, a demotion landing after the
/// last periodic refresh still let an already-open stream project an
/// admin-only report for up to another `LABEL_REFRESH_EVERY` (60s), since
/// `cached` alone would not see the demotion until its own next tick.
/// Revalidating only for this one content class keeps every other event on
/// the cheap cached read — no store lookup added to the hot path.
async fn is_admin_for_item(
    item: &EventStreamItem,
    runtime: &CompanyRuntime,
    actor: Option<&Actor>,
    cached: bool,
) -> bool {
    if is_owner_fallback_report(item) {
        refreshed_is_admin(runtime, actor, cached).await
    } else {
        cached
    }
}

/// Projects a live subscription item into the operator stream's safe wire
/// shape. A gap is an unpersisted control frame, deliberately structural-only.
fn project_stream_item_for_viewer(
    item: &EventStreamItem,
    authors: &std::collections::HashMap<String, String>,
    viewer: &Viewer,
    is_admin: bool,
) -> Option<serde_json::Value> {
    match item {
        EventStreamItem::Event(stored) => {
            project_event_for_viewer(stored, authors, viewer, is_admin)
        }
        EventStreamItem::Gap { missed } => Some(serde_json::json!({
            "type": "stream_gap",
            "missed": missed,
        })),
    }
}

/// Projects a stored event into the safe SSE wire shape, or `None` to drop it.
///
/// The projection is deny-by-default: every emitted object carries only
/// domain fields that already exist on the [`CompanyEvent`], and any variant not
/// explicitly listed — `OperatorMessage` (the operator's own echo),
/// `WebhookReceived` / `A2aTaskReceived` (raw third-party payloads),
/// `ScheduleFired`, `FeedbackFiled`, `MemoryFactDeleted`, `ReactionToggled` —
/// is dropped so nothing unexpected (or secret-bearing) ever reaches the
/// console. `ReactionToggled` is dropped on purpose rather than by oversight
/// (issue #364): a reaction carries the reacting *person*, this stream has no
/// per-viewer projection to resolve one into a label, and a reaction is
/// reload-visible, which is all the issue asks for. The actor (`by`) on
/// `ApprovalResolved` / `LifecycleChanged` is intentionally omitted: the console
/// renders the attention item without it, and it can carry a user id.
///
/// Adding a variant to [`CompanyEvent`] therefore drops it by default; it
/// reaches the console only by being listed here on purpose.
///
/// [`Viewer::Operator`] is always admin here, same as `Chat.history`'s
/// GraphQL resolver treats the platform bearer (issue #1781 review, Codex
/// P1) — this test helper's callers all use that viewer.
#[cfg(test)]
fn project_event(stored: &StoredEvent) -> Option<serde_json::Value> {
    project_event_for_viewer(
        stored,
        &std::collections::HashMap::new(),
        &Viewer::Operator,
        true,
    )
}

/// `is_admin` gates an owner-fallback `AgentReply` — journaled under
/// [`OWNER_FALLBACK_REPORT_AUTHOR`](crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR)
/// — the same way [`history_for_desk`](crate::server::chat_history::history_for_desk)
/// already gates it for a reload (issue #1781 review, Codex P1): a non-admin
/// viewer must never see the admin-only report just because it landed while
/// their SSE stream was open. The row is dropped outright rather than
/// projected with a redacted body — this stream has no partial-reveal shape
/// for any other event either, and a live listener that cannot see the row on
/// reload should not see it live.
fn project_event_for_viewer(
    stored: &StoredEvent,
    authors: &std::collections::HashMap<String, String>,
    viewer: &Viewer,
    is_admin: bool,
) -> Option<serde_json::Value> {
    use serde_json::json;

    let envelope = |ty: &str| {
        json!({
            "type": ty,
            "seq": stored.seq.value(),
            "atMillis": stored.at_millis,
        })
    };

    let value = match &stored.event {
        CompanyEvent::AgentReply {
            chat_id,
            agent_id,
            text,
            steps,
            outputs,
            task_id,
            parent,
            mentions,
            ..
        } => {
            // See this fn's doc: an owner-fallback report is admin-only, live
            // exactly as it is on reload (issue #1781 review, Codex P1).
            if !is_admin && agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR {
                return None;
            }
            let mut o = envelope("agent_reply");
            o["chatId"] = json!(chat_id);
            o["agentId"] = json!(agent_id);
            // The same pair `MessageView` ships on reload: the operator-facing
            // body, and the body as the model wrote it. They differ only on a
            // desk that deliberates, where `readable_moves` turns
            // `!support #topic ^3` into prose — see `MessageView::cue_text`.
            //
            // Order matters here, and cost a PR to learn: the grammar has to
            // reach `cueText` before `text` may lose it. The fold reads the
            // room's moves to know an episode happened at all, so cleaning
            // `text` while it was the only body on this frame took the
            // deliberation panel with it.
            o["cueText"] = json!(text);
            // What the operator reads, rewritten exactly as the reload already
            // rewrites it. A room's grammar is addressed to the fold, and the
            // journal keeps it — a room whose own transcript had been cleaned
            // could not count itself — so this lives at the display edge and
            // nowhere earlier. Every agent-facing path (`EpisodePrompt`,
            // `elsewhere_for`, `referral_prompt`, `chat_seed`) still reads the
            // stored line, which is how a seat can cite `^16` against a row it
            // can identify.
            //
            // A reply carrying no move is returned unchanged, which is every
            // reply on every desk that does not deliberate.
            o["text"] = json!(crate::server::chat_history::readable_moves(text.clone()));
            // Keys rework #2306, round-2 review KR-L2-03: re-classifies the
            // same bare X9 sentence `spawn_chat_turn` wrote into `text` for
            // exactly this class of failure. Omitted (reads as absent/false)
            // for every ordinary reply and every other failure class, so the
            // legacy frame shape is unchanged for them.
            if let Some(resolution) = crate::company::inference::copy::classify(text) {
                o["userFacing"] = json!(true);
                o["code"] = json!(resolution.code);
                o["message"] = json!(resolution.message);
                if let Some(id) = &resolution.pair_agent_id {
                    o["pairAgentId"] = json!(id);
                }
                if let Some(slug) = &resolution.provider_slug {
                    o["providerSlug"] = json!(slug);
                }
            }
            // Issue #364: which thread inside the channel this reply belongs
            // to, so a console watching live folds it under the same row a
            // reload would. Omitted for a reply in the channel itself, so the
            // legacy frame is unchanged.
            if let Some(parent) = parent {
                o["parentId"] = json!(parent.value().to_string());
            }
            // Scrubbed timeline (same shape the POST body carries); omitted
            // when empty so a tool-less reply's wire form is unchanged.
            if !steps.is_empty() {
                o["steps"] = json!(steps);
            }
            if !outputs.is_empty() {
                o["outputs"] = json!(outputs);
            }
            // Correlation key for a dispatch-produced reply (#185); omitted for
            // an ordinary chat reply so the legacy wire shape is unchanged.
            if let Some(task_id) = task_id {
                o["taskId"] = json!(task_id);
            }
            // Project the same viewer-relative metadata as chat/history. The
            // stream must carry complete ChatMentionDto values because the live
            // row is already durable and hydration intentionally skips it.
            let projected = project_mentions(mentions, authors, viewer);
            if !projected.is_empty() {
                o["mentions"] = json!(
                    projected
                        .into_iter()
                        .map(ChatMentionDto::from)
                        .collect::<Vec<_>>()
                );
            }
            o
        }
        CompanyEvent::TaskDispatched {
            task_id,
            origin_chat_id,
            origin_parent,
            ..
        } => {
            let mut o = envelope("task_dispatched");
            o["taskId"] = json!(task_id);
            // **Where the work was asked for, so the asking thread can say it
            // is running.**
            //
            // `desk_task_completed` below has carried this pair since #1890 B,
            // which is why a finished dispatch lands in the thread that raised
            // it. The start carried neither, so a thread that dispatched went
            // quiet the moment it did: the chat turn had genuinely succeeded —
            // it handed the work over — so its working row settled, and every
            // frame that followed was board-shaped and named only a card.
            // Minutes of a real agent turn rendered as nothing at all, then a
            // reply from nowhere.
            //
            // Omitted rather than null on exactly the terms the completion's
            // half uses, and read the same way: a missing `chatId` is a
            // board-created dispatch that belongs to no conversation, and a
            // missing `parentId` beside a present `chatId` is the channel
            // itself.
            if let Some(chat_id) = origin_chat_id {
                o["chatId"] = json!(chat_id);
            }
            if let Some(parent) = origin_parent {
                o["parentId"] = json!(parent.value().to_string());
            }
            o
        }
        // Issue #464: the frame the board was missing. Every other task event
        // here describes a card that already exists, so a card *opened* — by
        // chat intake, a delegation, the publish drain, the REST route —
        // reached the console only on its next reload.
        //
        // Three keys, all structural, and every one of them already reachable
        // by the same operator through `GET {scope}/tasks`. There is
        // deliberately no title and no note: a card's text is operator- or
        // agent-authored free text, and this frame's whole job is to say
        // *something moved*, not to carry the board. The console reacts by
        // re-reading the board it already knows how to read, which keeps the
        // card's content on exactly one route instead of two.
        // Issue #327: the frame the Workspace tab was missing. This stream is
        // deny-by-default — an event with no arm here is simply not projected —
        // so without this the tab stays on refresh-and-refocus no matter what
        // the store announces.
        //
        // Two keys, both structural, and both already reachable by the same
        // operator through `GET {scope}/workspace`. There is deliberately **no
        // node name and no body**: a note's text is operator- or agent-authored
        // free text, and this frame's whole job is to say *something moved*.
        // The console reacts by re-reading the tree it already knows how to
        // read, which keeps the workspace's content on exactly one route.
        CompanyEvent::WorkspaceChanged { node_id, change } => {
            let mut o = envelope("workspace_changed");
            o["nodeId"] = json!(node_id);
            o["change"] = json!(change);
            o
        }
        CompanyEvent::TaskCardChanged {
            task_id,
            change,
            column,
        } => {
            let mut o = envelope("task_card_changed");
            o["taskId"] = json!(task_id);
            o["change"] = json!(change);
            // Omitted rather than null on a removal, so "gone" is a presence
            // check on the console rather than a null check.
            if let Some(column) = column {
                o["column"] = json!(column);
            }
            o
        }
        // `message` is scrubbed at the source (`OcMcpCallTool` → `HarnessBrain`
        // drain), so it can never carry a credential, response body, or URL query
        // string — safe to forward verbatim. See `CompanyEvent::McpCallFailed`.
        CompanyEvent::McpCallFailed {
            server,
            tool,
            status,
            message,
            task_id,
        } => {
            let mut o = envelope("mcp_call_failed");
            o["server"] = json!(server);
            o["tool"] = json!(tool);
            o["status"] = json!(status);
            o["message"] = json!(message);
            // Correlation key when the failing call ran inside a dispatch
            // (#185); omitted for a chat-turn failure.
            if let Some(task_id) = task_id {
                o["taskId"] = json!(task_id);
            }
            o
        }
        // The dispatch terminal (#185), narrowed and widened by #377.
        //
        // **Widened** with `chatId`: the conversation the card was raised from,
        // which is what lets a console file the settle into the channel the
        // work came from instead of guessing at one. Omitted rather than null
        // when the card names none — mirroring `approval_parked` below, so "no
        // conversation raised this" is a presence check on the console rather
        // than a null check, and a board-created card is board-only on the wire
        // too.
        //
        // **Narrowed** by dropping `output`. The run's prose already reaches
        // the operator as the orchestrator's relay bubble (#151); what the
        // channel was missing is the structural fact that the card *settled*
        // and *where*, which is `column`. Carrying the prose here as well would
        // put one run's words into the same channel twice, so it is dropped at
        // the projection — the one place that can guarantee no later reader
        // reintroduces the duplicate. Nothing in the console read it. `desk`
        // stays for wire compatibility.
        // **A crossing happened; the thread it belongs to must be re-read.**
        //
        // A crossing renders as a collapsed `referralConversation` folded onto
        // the asking row, and `attach_referral_origins` — which builds it — runs
        // in `history_for_desk` and nowhere else. So a crossing was invisible
        // live and appeared only once something re-read the thread, which for a
        // desk crossing meant waiting for the turn to settle and for a pair DM
        // meant never: its rows are journaled in the pair's own `dm:<a>+<b>`
        // conversation, which no desk view is watching.
        //
        // This frame carries no crossing content, on purpose. Rebuilding the
        // fold here would be a second implementation of a rule this subsystem
        // has already had to fix in two places four separate times; the reload
        // projection is the authority and this only tells the console to ask it
        // again. Same shape as #2329's remedy for the grammar split: add to the
        // stream rather than teach it to recompute.
        CompanyEvent::ReferralEnqueued {
            from_desk,
            trigger_sequence,
            to_desk,
            target,
            asker,
            conversation,
            returning,
            ..
        } => {
            let mut o = envelope("referral");
            // **The desk whose transcript gains the fold, which is not the same
            // field on both legs.**
            //
            // The fold lands on the ASKING row, and a return leg is addressed
            // the other way round: `mark` builds it from the answering desk, so
            // its `from_desk` is the far desk and `to_desk` is the desk that
            // asked. Emitting `from_desk` unconditionally pointed the console at
            // the far desk exactly on the leg that carries the answer — and
            // since the forward frame goes out before an answer exists, a
            // cross-desk crossing was never refreshed on the desk waiting for it
            // (Codex, #2341).
            o["chatId"] = json!(match returning {
                true => to_desk,
                false => from_desk,
            });
            // The row it folds onto, so a console need not re-read a whole desk
            // to find what changed.
            o["sequence"] = json!(trigger_sequence);
            o["toDesk"] = json!(to_desk);
            o["target"] = json!(target);
            o["asker"] = json!(asker);
            // A pair thread exists only for a person-to-person crossing; on a
            // desk crossing `target` is merely the room's first eligible seat.
            o["direct"] = json!(conversation.is_some());
            // Which leg this is, read the same way `ReferredFrom::returning`
            // is: a return is the one that completes the exchange, so a console
            // that only wants to re-read once can wait for it.
            o["returning"] = json!(returning);
            o
        }
        CompanyEvent::DeskTaskCompleted {
            task_id,
            desk,
            column,
            origin_chat_id,
            origin_parent,
            ..
        } => {
            let mut o = envelope("desk_task_completed");
            o["taskId"] = json!(task_id);
            o["desk"] = json!(desk);
            o["column"] = json!(column);
            if let Some(chat_id) = origin_chat_id {
                o["chatId"] = json!(chat_id);
            }
            // **Widened again** by #1890 B, with the thread inside that
            // channel. Omitted rather than null on exactly the terms `chatId`
            // is, and read the same way: absent means the channel-level
            // conversation, which is where every marker landed before.
            //
            // The live frame and `chat/history`'s rehydrated twin must agree on
            // this or the marker would render inline live and jump into a
            // thread on reload — the split the `h<seq>` identity dedupe exists
            // to prevent. Stringified for the same reason the history
            // projection's `parentId` is: the console keys threads by message
            // id, and a message id is a string there.
            if let Some(parent) = origin_parent {
                o["parentId"] = json!(parent.value().to_string());
            }
            o
        }
        // Issue #379: a request just parked, so a console watching the
        // conversation it came from can raise the card live instead of waiting
        // for its next approvals poll.
        //
        // Three keys and no more — deny-by-default, like every other arm here.
        // No `payload`: the effect's arguments are redacted exactly once, in
        // `pending_approvals`, and this frame deliberately does not become a
        // second place that has to. No `agent` either: the console reads the
        // asker off the same refreshed summary. What is here is only enough to
        // decide *whether* to refresh and *where* the card belongs.
        CompanyEvent::ApprovalParked {
            approval_id,
            effect_kind,
            thread,
        } => {
            let mut o = envelope("approval_parked");
            o["approvalId"] = json!(approval_id.as_ref());
            o["kind"] = json!(effect_kind);
            // Omitted when no conversation produced it, so a page-only approval
            // is page-only on the wire too.
            if let Some(thread) = thread {
                o["chatId"] = json!(thread);
            }
            o
        }
        // The actor is still dropped — see the deny-by-default note above and
        // `projects_approval_resolved_without_the_actor`. What crosses the wire
        // is one bit derived from it.
        CompanyEvent::ApprovalResolved {
            approval_id,
            verdict,
            by,
        } => {
            let mut o = envelope("approval_resolved");
            o["approvalId"] = json!(approval_id.as_ref());
            o["verdict"] = json!(verdict);
            // Issue #971: say when the HOST resolved it, not a person.
            //
            // An expiry appends `ApprovalResolved { verdict: Deny, by: System }`
            // — a default-deny on silence, which is a real resolution and has
            // to be one (#305, #469). But this frame carried only the verdict,
            // so the console toasted **"Approval denied"** and an operator was
            // told they had declined something they never saw. That was a rare
            // false attribution while the deadline was a week; shortening it to
            // 24 hours makes it routine, which is what turns a latent wording
            // bug into a defect worth fixing in the same change.
            //
            // **A flag, deliberately not `by`.** Sending the actor would be the
            // obvious fix and is the wrong one: the projection is deny-by-default
            // and an assertion below pins that no actor and no user id reaches
            // this feed. A boolean derived from `by.kind` answers the console's
            // question — "did a person decide this?" — while carrying nothing
            // identifying. That assertion is EXTENDED to cover this field, never
            // replaced.
            //
            // Skipped when false, so an operator's own decision serializes
            // exactly as it did before and an old console is unaffected.
            if by.kind == ActorKind::System {
                o["automatic"] = json!(true);
            }
            o
        }
        CompanyEvent::LifecycleChanged { from, to, .. } => {
            let mut o = envelope("lifecycle_changed");
            o["from"] = json!(from);
            o["to"] = json!(to);
            o
        }
        CompanyEvent::PaymentReceived { amount_usd, memo } => {
            let mut o = envelope("payment_received");
            o["amountUsd"] = json!(amount_usd);
            o["memo"] = json!(memo);
            o
        }
        // Issue #112: surface a newly authored workflow so the console can react
        // live (e.g. refresh the Workflows tab). Only the id + display name go on
        // the wire — the actor (`by`) is omitted, matching the deny-by-default
        // projection of the other attributed events.
        CompanyEvent::WorkflowCreated {
            workflow_id, name, ..
        } => {
            let mut o = envelope("workflow_created");
            o["workflowId"] = json!(workflow_id);
            o["name"] = json!(name);
            o
        }
        // Issue #259: an edited or removed workflow, so a console holding the
        // Workflows tab open re-reads the picker instead of offering a graph
        // that changed under it (or one that no longer exists). Same two fields
        // and same deny-by-default actor omission as `workflow_created` — and,
        // as the variant docs spell out, there is no graph body to leak here.
        CompanyEvent::WorkflowUpdated {
            workflow_id, name, ..
        } => {
            let mut o = envelope("workflow_updated");
            o["workflowId"] = json!(workflow_id);
            o["name"] = json!(name);
            o
        }
        CompanyEvent::WorkflowDeleted {
            workflow_id, name, ..
        } => {
            let mut o = envelope("workflow_deleted");
            o["workflowId"] = json!(workflow_id);
            o["name"] = json!(name);
            o
        }
        // The structural changes a console draws its activity graph from: who
        // created a teammate or a desk, who moved a seat, who changed how a desk
        // deliberates. Before these the graph could only infer a spawn from a
        // redacted tool-call frame that does not survive a reload.
        //
        // `by_agent_id` rides along and `by` does not — the same deny-by-default
        // actor omission as every arm above. The agent is the company's own
        // structure and is what the edge is drawn from; the human is not.
        CompanyEvent::TeammateAdded {
            agent_id,
            role,
            by_agent_id,
            ..
        } => {
            let mut o = envelope("teammate_added");
            o["agentId"] = json!(agent_id);
            o["role"] = json!(role);
            if let Some(by) = by_agent_id {
                o["byAgentId"] = json!(by);
            }
            o
        }
        CompanyEvent::DeskCreated {
            desk_id,
            name,
            members,
            ..
        } => {
            let mut o = envelope("desk_created");
            o["deskId"] = json!(desk_id);
            o["name"] = json!(name);
            o["members"] = json!(members);
            o
        }
        CompanyEvent::DeskDeleted { desk_id, .. } => {
            let mut o = envelope("desk_deleted");
            o["deskId"] = json!(desk_id);
            o
        }
        CompanyEvent::DeskMembersChanged {
            desk_id,
            added,
            removed,
            ..
        } => {
            let mut o = envelope("desk_members_changed");
            o["deskId"] = json!(desk_id);
            o["added"] = json!(added);
            o["removed"] = json!(removed);
            o
        }
        CompanyEvent::DeskHiveConfigured { desk_id, reset, .. } => {
            let mut o = envelope("desk_hive_configured");
            o["deskId"] = json!(desk_id);
            o["reset"] = json!(reset);
            o
        }
        // Issue #276: a workflow armed or paused, so a console holding the
        // Workflows tab open re-renders the toggle instead of showing a stale
        // one — and so an operator watching the stream sees the disarm rule fire
        // on someone else's edit. `reason` rides along because it is a closed
        // enum of our own words with no operator content in it, and it is the
        // difference between "a colleague paused this" and "the host refused to
        // arm it"; `by` is dropped, same deny-by-default actor omission as every
        // arm above.
        CompanyEvent::WorkflowEnabledChanged {
            workflow_id,
            name,
            enabled,
            reason,
            ..
        } => {
            let mut o = envelope("workflow_enabled_changed");
            o["workflowId"] = json!(workflow_id);
            o["name"] = json!(name);
            o["enabled"] = json!(enabled);
            o["reason"] = json!(reason);
            o
        }
        // Issue #111: surface an accepted operator steer so the console's
        // in-flight strip can refresh live. Only the task id + action word go on
        // the wire — the actor (`by`) and the operator's redirect `instruction`
        // are dropped, matching the deny-by-default projection.
        CompanyEvent::TaskSteered {
            task_id, action, ..
        } => {
            let mut o = envelope("task_steered");
            o["taskId"] = json!(task_id);
            o["action"] = json!(action);
            o
        }
        // Issue #228: a finished workflow run, so the console can toast a
        // report that did not go out *while it is happening* instead of only on
        // the next reload of the history panel.
        //
        // This widens nothing. The projected fields are exactly what the run
        // drawer already renders, and every one of them — `target` included —
        // already reaches this same console in the manual run's HTTP response
        // (see `RunWorkflowResponse` in `super::ops::workflows`). The stream is
        // operator-authenticated and company-scoped, like that response.
        //
        // `runId` was not projected before issue #371, because it was always
        // `None` and emitting it would have put a permanently-null key on the
        // wire. Now every entry point mints one, and the console needs it: it is
        // what ties this settle-frame to the progress frames it has been
        // painting, so a cron run finishing mid-manual-run clears the right
        // canvas. Still omitted when absent, for the pre-#371 rows.
        CompanyEvent::WorkflowRunFinished {
            workflow_id,
            scheduled,
            run_id,
            deliveries,
            pending_approvals,
            error,
            cancelled,
            notices,
            board,
            blocked_nodes,
            approvals,
        } => {
            let mut o = envelope("workflow_run_finished");
            o["workflowId"] = json!(workflow_id);
            o["scheduled"] = json!(scheduled);
            o["deliveries"] = json!(deliveries);
            o["pendingApprovals"] = json!(pending_approvals);
            if let Some(run_id) = run_id {
                o["runId"] = json!(run_id);
            }
            // Omitted rather than null on a run that finished, so the console's
            // "did this fail?" check is a presence check.
            if let Some(error) = error {
                o["error"] = json!(error);
            }
            // Issue #383: same presence-check discipline. A run stopped by an
            // operator carries no `error`, so without this frame the console
            // could only render it as an ordinary clean finish — and the
            // operator who just pressed Cancel would get "ran successfully".
            if *cancelled {
                o["cancelled"] = json!(true);
            }
            // Issue #638: same presence-check discipline again. Omitted on the
            // overwhelming majority of runs, which raise nothing — so a console
            // that checks for the key gets "was there anything to tell me?"
            // without having to compare against an empty list.
            if !notices.is_empty() {
                o["notices"] = json!(notices);
            }
            // Issue #661 (M5): the same presence-check discipline once more, and
            // the same widens-nothing argument as `deliveries` above — in fact a
            // weaker claim, because a board row is structural by construction (see
            // `WorkflowRunBoardRow`) rather than by this arm choosing what to
            // forward. It carries ids and the card's own title, which the board
            // read already serves this same console under the same guard.
            //
            // Projected so a console watching a run live learns it opened a card at
            // the moment it settles, rather than only on the next history read.
            if !board.is_empty() {
                o["board"] = json!(board);
            }
            // Issues #881 / #880: the same presence-check discipline again, and
            // projected for the same reason `deliveries` is — a console
            // watching a run live must not be told it finished cleanly while
            // the history it reloads a moment later says it blocked. Both rows
            // are structural by construction (node ids, tool names, approval
            // ids), so this arm forwards no payload it has to choose to scrub.
            if !blocked_nodes.is_empty() {
                o["blockedNodes"] = json!(blocked_nodes);
            }
            if !approvals.is_empty() {
                o["approvals"] = json!(approvals);
            }
            o
        }
        // Issue #371: the live half of per-node progress. This is what turns the
        // console from "the button spins" into "node 3 of 6 just finished" —
        // and it costs the wire nothing it did not already carry, because every
        // projected field is structural.
        //
        // There is nothing to scrub here and that is by construction, not by
        // omission: the events themselves carry no node output and no error
        // text (see `CompanyEvent::WorkflowNodeFinished`), so this arm could not
        // leak a payload even if it forwarded the event wholesale. Contrast the
        // `workflow_run_finished` arm above, which has to *choose* what to
        // forward because its event carries operator-only delivery rows.
        CompanyEvent::WorkflowRunStarted {
            workflow_id,
            run_id,
            scheduled,
            started_by,
            ..
        } => {
            let mut o = envelope("workflow_run_started");
            o["workflowId"] = json!(workflow_id);
            o["runId"] = json!(run_id);
            o["scheduled"] = json!(scheduled);
            // Issue #1862 prerequisite: forwarded only when present, so a run
            // journaled before this field existed (or one that genuinely has
            // no sender) projects exactly as it did before.
            if let Some(started_by) = started_by {
                o["startedBy"] = json!(started_by);
            }
            o
        }
        // Issue #382: the live per-node START bracket, the counterpart of the
        // finish arm below. Without an explicit arm here it would fall to the
        // `_ => return None` wildcard and be silently dropped, and the canvas
        // would be back to deriving "currently executing" from graph topology —
        // the exact guess #382 replaces. Structural by construction: the event
        // carries only ids, so nothing to scrub.
        CompanyEvent::WorkflowNodeStarted {
            workflow_id,
            run_id,
            node_id,
        } => {
            let mut o = envelope("workflow_node_started");
            o["workflowId"] = json!(workflow_id);
            o["runId"] = json!(run_id);
            o["nodeId"] = json!(node_id);
            o
        }
        CompanyEvent::WorkflowNodeFinished {
            workflow_id,
            run_id,
            node_id,
            status,
            elapsed_ms,
            // Issue #1014: the null-resolved config paths ride the durable event
            // and the run-response history, but the live operator SSE frame
            // stays the three structural scalars it already was — the console
            // surfaces diagnostics from the run-detail drawer, not this stream.
            diagnostics: _,
            agent_run_id,
        } => {
            let mut o = envelope("workflow_node_finished");
            o["workflowId"] = json!(workflow_id);
            o["runId"] = json!(run_id);
            o["nodeId"] = json!(node_id);
            o["status"] = json!(status);
            o["elapsedMs"] = json!(elapsed_ms);
            // A fourth structural id, on the same terms as the three above: it
            // is reachable by this same operator through `GET {scope}/runs`, and
            // it is what lets a console watching the canvas open the node's step
            // trace directly rather than searching for which attempt was its.
            // Omitted entirely when the node opened none, so a frame for a
            // non-agent node is byte-identical to what it was.
            if let Some(agent_run_id) = agent_run_id {
                o["agentRunId"] = json!(agent_run_id);
            }
            o
        }
        // Issue #983: a turn was accepted, so a console watching the
        // conversation can show it as under way instead of showing the
        // operator's question with nothing after it. Three keys, all
        // structural, and every one already reachable by the same operator
        // through `GET {scope}/runs`.
        //
        // Deliberately **no message text and no actor**: the text is on the
        // `OperatorMessage` this brackets — which stays dropped, see the
        // module note above — and `by` is a user id, dropped here exactly as
        // every other attributed arm drops it. The console reacts by reading
        // the row it already knows how to read.
        CompanyEvent::TurnStarted {
            turn_id,
            chat_id,
            parent,
            ..
        } => {
            let mut o = envelope("turn_started");
            o["turnId"] = json!(turn_id);
            o["chatId"] = json!(chat_id);
            // Omitted rather than null for a turn answering the channel
            // itself, so "is this in a thread?" is a presence check — the same
            // discipline `agent_reply` above uses for the same field.
            if let Some(parent) = parent {
                o["parentId"] = json!(parent.value().to_string());
            }
            o
        }
        // The closing bracket. Structural for a sharper reason than its
        // sibling: the event's `error` is a failure reason in our own words
        // that can name internals, and this stream is the one place it must
        // not be forwarded to. A console learns *that* the turn is over here
        // and reads *why* from the run row, which is tenant-scoped.
        CompanyEvent::TurnFailed { turn_id, .. } => {
            let mut o = envelope("turn_settled");
            o["turnId"] = json!(turn_id);
            o
        }
        // Issue #1015: the push half of attempt status. Structural, and for the
        // same sharper reason as `turn_settled` directly above — `error` is a
        // failure reason in our own words that can name internals, so the
        // console learns *that* the attempt moved here and reads *why* from the
        // run row, which is tenant-scoped.
        //
        // `from` rides along so a consumer holding a row can tell a live frame
        // from a replayed or out-of-order one, which a bare `to` cannot. It is
        // omitted rather than null on the mint, where there is no prior state —
        // the same presence-check discipline `turn_started`'s `parentId` uses.
        CompanyEvent::RunStatusChanged {
            run_id,
            task_id,
            attempt,
            from,
            to,
            ..
        } => {
            let mut o = envelope("run_status_changed");
            o["runId"] = json!(run_id);
            o["attempt"] = json!(attempt);
            o["status"] = json!(to);
            if let Some(task_id) = task_id {
                o["taskId"] = json!(task_id);
            }
            if let Some(from) = from {
                o["from"] = json!(from);
            }
            o
        }
        // Not an attention signal, or carries a raw payload we never put on the
        // wire — dropped.
        _ => return None,
    };
    Some(value)
}

fn lookup(state: &AppState, id: &str) -> Result<Arc<CompanyRuntime>, ApiError> {
    state
        .registry()
        .get(&CompanyId::new(id))
        .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(id.to_string())))
}

fn sole(state: &AppState) -> Result<Arc<CompanyRuntime>, ApiError> {
    state.registry().sole().ok_or_else(|| {
        ApiError(OpenCompanyError::CompanyNotFound(
            "single-company".to_string(),
        ))
    })
}

/// `GET /api/v1/companies` — status of every company this principal may see.
///
/// A platform-scope token sees all of them; a tenant token sees only the
/// companies it owns; a user sees their own company and nothing else — not even
/// that others exist on this host.
async fn list_companies(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<CompanyStatus>>, ApiError> {
    let mut out = Vec::new();
    // `visible_companies` is the one place this filter lives, shared with the
    // GraphQL root, so REST and GraphQL cannot disagree about who sees what.
    for id in auth.visible_companies(&state) {
        if let Some(runtime) = state.registry().get(&id) {
            out.push(runtime.status().await?);
        }
    }
    Ok(Json(out))
}

/// `GET /api/v1/companies/{id}` — one company's status.
async fn company_status(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CompanyStatus>, crate::server::Rejection> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp.into());
    }
    let runtime = lookup(&state, &id)?;
    runtime
        .status()
        .await
        .map(Json)
        .map_err(|e| ApiError(e).into_response().into())
}

/// The operator's chat request body.
///
/// WS3 extends the Phase-1 `{text}` body with an optional `chat` desk id
/// (single-responder in v1): replies are journaled against that desk so the
/// GraphQL `Chat.history` resolver can read them back. The field is accepted
/// under either `text` (Phase-1) or `message` (the console) key.
#[derive(Debug, Deserialize)]
struct ChatMessage {
    /// The operator's message text.
    #[serde(alias = "message")]
    text: String,
    /// The desk the message is addressed to. Defaults to the "General" desk.
    #[serde(default)]
    chat: Option<String>,
    /// The message this one replies to, by its id (issue #364) — a thread reply
    /// rather than a new line in the channel.
    ///
    /// A string, not a number, because that is what every other message id on
    /// this API is: `chat/history` returns `id: "42"`, and a console that had to
    /// remember which surface wanted which type would eventually get it wrong.
    /// Parsed to a sequence position here, and a value that is not one is a 400
    /// rather than a silently-dropped thread.
    #[serde(default)]
    parent: Option<String>,
    /// What this message is **for** (issues #580, #1152) — whether an
    /// actionable request opens a one-off card or a workflow card, or whether
    /// the operator is saying it is not a request for work at all.
    ///
    /// The operator chooses explicitly (decision D2a); absent means `once`, so
    /// an ordinary chat request is unchanged. `once` and `workflow` are only
    /// consulted when the message actually carries a task intent — a greeting
    /// or a question opens no card regardless; `chat` is consulted whatever the
    /// triage said, because withholding is the whole point of it.
    ///
    /// **One field, one choice.** The `chat` word rides the existing
    /// `deliverable` key rather than arriving as a second `intent` field, so a
    /// body cannot assert "build me the workflow" and "just chatting" about the
    /// same message — the split-brain #1035 closed, pointed the other way.
    #[serde(default)]
    deliverable: Option<crate::ports::types::MessageIntent>,
    /// Return as soon as the turn has been accepted and given an id, instead of
    /// holding the request open for the whole turn (issue #983).
    ///
    /// A turn's duration is unbounded, so the synchronous shape is broken by
    /// construction and no timeout value fixes it: five concurrent messages
    /// queued on the per-company serial lock all 504'd at the edge while the
    /// work ran on invisibly. This is the response path that removes the wait —
    /// the turn is journaled and given a durable row before this returns, so the
    /// operator reads its progress and its answer back rather than holding a
    /// socket open for them.
    ///
    /// **Opt-in, and compatible in both directions.** A caller that omits it
    /// gets today's synchronous response byte-for-byte. A newer console talking
    /// to an *older* host sends it and the old host ignores the unknown field
    /// (this struct has no `deny_unknown_fields`) and answers the full
    /// synchronous 200 — which is exactly why the console must decide what
    /// happened from the response's **shape**, not from what it asked for.
    ///
    /// Deliberately not the default. A trivial turn settles in 4–6s, and a fast
    /// synchronous answer is genuinely better when it fits; the eventual right
    /// shape is a hybrid that answers synchronously up to N seconds and then
    /// hands back a 202, which needs this turn record to exist first.
    #[serde(default)]
    detach: bool,
    /// Who this message names, as the console's picker resolved them.
    ///
    /// Three states, and they are three different instructions:
    ///
    /// * **Absent** — the caller has no picker (`curl`, the API, a console
    ///   predating this field). The host extracts mentions from the text
    ///   itself, so `@engineer` still works from the command line.
    /// * **Present and non-empty** — the caller resolved these against a roster
    ///   it had loaded. Re-validated here against the live one and demoted, not
    ///   trusted; a stale picker must not be able to address a turn to a
    ///   teammate the company no longer has.
    /// * **Present and empty** — the caller ran its picker and found nothing.
    ///   Honoured as the answer it is: the host does not then guess on its
    ///   behalf and chip an `@word` the author deliberately left unresolved.
    ///
    /// Additive in both directions, on exactly the terms `detach` documents
    /// above: this struct has no `deny_unknown_fields`, so a newer console
    /// against an older host degrades to host-side extraction, and an older
    /// console against a newer host gets extraction too.
    #[serde(default)]
    mentions: Option<Vec<crate::ports::types::Mention>>,
    /// The workspace node ids of files attached to this message (issue #1682).
    ///
    /// **Ids only, and nothing else is trusted.** The client uploads each file
    /// first (`POST {scope}/chat/upload`), gets back a `node_id`, and lists
    /// those ids here. The host re-resolves each within this company's own
    /// workspace and takes the name / mime / size from the store — so a foreign
    /// or spoofed reference cannot cross a company boundary or misdescribe its
    /// payload (see `resolve_attachments`). Any file in the tree may be
    /// attached, however it was written; an id naming a folder, or naming
    /// nothing in this company, is a `400`.
    ///
    /// Additive in both directions: this struct has no `deny_unknown_fields`,
    /// so a newer console against an older host has its ids ignored and its
    /// message still posts, and an older console omits the field entirely — an
    /// absent list is an empty one, the exact pre-#1682 wire shape.
    #[serde(default)]
    attachments: Vec<String>,
}

/// A chat or approval-resolution response: the company's channel replies.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatResponse {
    /// Channel responses produced by the cycle.
    responses: Vec<OutboundMessage>,
    /// The durable id the operator's own message was journaled under (issue
    /// #364), so the console can replace the local id it minted optimistically
    /// with one a reload — or another operator — can resolve.
    ///
    /// Omitted when the cycle journaled nothing, and by every host predating
    /// this field; a console that finds it missing knows not to offer actions
    /// that would need it.
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    /// The same count [`ResolveReceiptDto::still_awaiting`] carries, for the
    /// non-detached resolve the Approvals page makes (issue #561).
    ///
    /// Only ever set by a resolve. Omitted everywhere else — a chat turn is not
    /// blocked on anybody's sign-off — so no other caller has to learn it
    /// exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    still_awaiting: Option<usize>,
    /// The durable turn row this message opened (issue #983), additive on the
    /// synchronous response exactly as `runId` was added to the workflow run
    /// response — so a caller that never asked to detach can still read the
    /// turn back from `GET {scope}/runs/{turn_id}` afterwards.
    ///
    /// `None` when the run store refused to mint a row: record-keeping does not
    /// get to fail the work it records, so the turn still ran and still answered
    /// here. A caller that finds it missing has the reply in hand anyway.
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    /// The same discriminator [`ResolveReceiptDto::outcome`] carries, for the
    /// non-detached resolve the Approvals page makes (issue #1449).
    ///
    /// The page never sees a `ResolveReceiptDto` — that shape is the *detached*
    /// answer, which only the inline chat card asks for — so without this the
    /// one surface the bug was reproduced on had no way to learn its click had
    /// been refused, whatever the receipt said.
    ///
    /// Only ever set by a resolve, and omitted by every host predating it, which
    /// a console reads as "this host cannot tell me" and words its confirmation
    /// exactly as it did before rather than guessing.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    /// Set when a thread reply was intercepted as review feedback on an
    /// `in_review` dispatch card and re-dispatched it, rather than answered
    /// with `responses` here (Codex #3903907771). The re-run's own reply
    /// still arrives later on the event stream and in `chat/history` — this
    /// only tells the console not to read an empty `responses` as "the turn
    /// produced nothing."
    ///
    /// Omitted (not `false`) on every other response, so a host predating
    /// this field is indistinguishable from one that never took this branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    review_feedback_applied: Option<bool>,
    /// The same list [`ResolveReceiptDto::settled_ids`] carries, for the
    /// non-detached resolve the Approvals page makes: a blocker answered there
    /// settles its whole root-cause group, and the page owes those siblings the
    /// same removal it gives the card that was clicked.
    #[serde(skip_serializing_if = "Option::is_none")]
    settled_ids: Option<Vec<String>>,
}

/// The `detach: true` response (issue #983): the turn's id and the durable id of
/// the operator's own message, handed back before the cycle has taken the
/// per-company lock.
///
/// **`detached` is the discriminator, and it is a constant `true` on purpose.**
/// A newer console pointed at an older host sends `detach` and gets the *full
/// synchronous* body back, because the old host ignores the unknown field. So
/// the console cannot tell the two apart by what it asked for — only by what
/// came back. `responses` present means the turn already settled; `detached`
/// present means read it back. A field that is only ever `true` is what makes
/// that a presence check rather than a guess.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DetachedChatResponse {
    /// The turn's durable row, to poll on `GET {scope}/runs/{turn_id}`.
    ///
    /// Not optional, unlike [`ChatResponse::turn_id`]: this body is only ever
    /// produced when the row exists (the handler falls through to the
    /// synchronous settle when the run store refused one), because the console
    /// arms its poll from this id and that poll is the detached turn's sole
    /// delivery path when `/events` is buffered or unavailable.
    turn_id: String,
    /// The durable id the operator's own message was journaled under.
    ///
    /// Never optional here, unlike on the synchronous response: since issue #983
    /// the append happens at accept time, so by the time this body exists the
    /// message is already in the transcript. That is what lets the console
    /// reconcile its optimistic bubble immediately instead of at settle.
    message_id: String,
    detached: bool,
}

/// The two shapes `POST {scope}/chat` can answer with.
///
/// An enum rather than a bare [`Response`] so the two bodies stay typed and the
/// status codes live in one place: `200` for the settled turn the route has
/// always returned, `202 Accepted` for a turn that has been accepted and started
/// but has not finished — which is precisely what `202` means.
enum ChatOk {
    Settled(Box<ChatResponse>),
    Detached(DetachedChatResponse),
}

impl IntoResponse for ChatOk {
    fn into_response(self) -> Response {
        match self {
            Self::Settled(body) => Json(body).into_response(),
            Self::Detached(body) => (StatusCode::ACCEPTED, Json(body)).into_response(),
        }
    }
}

/// The canonical assignee for a card opened from a chat message: whoever the
/// thread was addressed to (issue #982).
///
/// `""` — today's unconditional behaviour, and still the answer for most
/// messages — for an unaddressed message, for a key that names nothing on the
/// roster, for an ambiguous one, and for a company record that will not load.
/// A teammate resolves to their canonical id; a **desk** resolves to the desk
/// id, never to its lead: a desk assignment is ownership, and
/// [`AssigneeResolution::canonical`] is where that invariant lives (issue #214),
/// so this reads it rather than restating it. An empty desk resolves to the desk
/// too, which dispatch refuses visibly with a reason — a better outcome than the
/// silent misroute this replaces.
///
/// The `dm:` fallback is tried **last** and only on a key that resolved to
/// nothing, so it can never take a thread that routes somewhere today.
async fn addressed_assignee(runtime: &Arc<CompanyRuntime>, chat: Option<&str>) -> String {
    use crate::runtime::assignee::{self, AssigneeResolution};

    let Some(chat) = chat.map(str::trim).filter(|c| !c.is_empty()) else {
        return String::new();
    };
    let company = match runtime.store().load(runtime.id()).await {
        Ok(Some(company)) => company,
        Ok(None) => {
            tracing::warn!(
                company = %runtime.id(),
                "no company record while assigning a chat card; leaving it unassigned"
            );
            return String::new();
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                company = %runtime.id(),
                "failed to read the roster while assigning a chat card; leaving it unassigned"
            );
            return String::new();
        }
    };
    let mut resolution = assignee::resolve(&company, chat);
    if matches!(resolution, AssigneeResolution::Unknown(_))
        && let Some(key) = assignee::dm_key(chat)
    {
        resolution = assignee::resolve(&company, key);
    }
    if let Some(reason) = resolution.rejection() {
        tracing::debug!(
            company = %runtime.id(),
            chat = %chat,
            reason = %reason,
            "[chat] the addressed thread names nobody the card can be handed to"
        );
    }
    resolution.canonical().unwrap_or_default().to_string()
}

/// Runs one operator-chat cycle, returning the report and, when a complaint
/// intent captured feedback, the note that was captured (so the caller can emit
/// the `feedback.created` webhook).
///
/// Takes the [`AcceptedTurn`] rather than a thread parent since issue #983: the
/// message this cycle runs is already journaled, so the parent it carries is a
/// fact about the event rather than something this function decides.
async fn run_chat(
    runtime: Arc<CompanyRuntime>,
    message: ChatMessage,
    by: Option<Actor>,
    accepted: &AcceptedTurn,
) -> Result<(CycleReport, Option<String>), ApiError> {
    // Re-checked here rather than only at accept: this is also reachable
    // directly, and a lifecycle can change between accepting a turn and running
    // it. `accept_chat_turn` runs the same check *before* the append, so a
    // refusal never leaves a message in the transcript that no turn answers.
    runtime.ensure_running().await?;
    // Whether this is a workflow copilot thread (issue #416): a conversation
    // ABOUT one graph, not a request to the company. Read once, because both
    // of the deterministic side effects below have to be suppressed for it.
    let confined = crate::company::copilot::is_copilot_thread(message.chat.as_deref());
    // Operator-chat feedback intent: a complaint phrase ("that was wrong — flag
    // it") captures a feedback item alongside the normal cycle. Neutral chat
    // carries no intent, so ordinary messages are untouched.
    //
    // Suppressed on a copilot thread for the same reason the card below is: "no,
    // that's wrong, this node keeps failing" is the operator correcting a
    // conversation about their graph, and filing it as company feedback would
    // record a complaint they did not make about work they were not discussing.
    let feedback_note = if let Some(category) = (!confined)
        .then(|| crate::feedback::detect_chat_intent(&message.text))
        .flatten()
    {
        runtime
            .capture_feedback(crate::feedback::FeedbackInput {
                category,
                note: message.text.clone(),
                work_ref: None,
                template_name: None,
                template_version: None,
            })
            .await?;
        Some(message.text.clone())
    } else {
        None
    };
    // A card from the composer is opened on ONE signal only: the operator
    // pressed the control named "Build me the workflow" (issue #845). That is a
    // positive statement of intent about this message, made by the person who
    // wrote it.
    //
    // There used to be a second, lexical signal here: `task_intent::triage_message`
    // read the message's words and opened a `todo`/`planning` card whenever it
    // led with an action verb ("build the landing page", "can you set up the
    // newsletter"). It is gone, along with its twin on the runtime side
    // (`DelegationRunner::open_direct_work_card`, which carded anything
    // "substantial" said to a desk or a teammate). Between them, nearly every
    // message typed into a desk became a board card that nobody had asked for,
    // and the agent answering it had no say in the matter. Tracking is now the
    // agent's own decision, made with a tool call — `spawn_task` opens a card,
    // and a hand-off through `delegate_to_desk` / `delegate_to_teammate` opens
    // the card that tracks the hand-off — so a card on the board means an agent
    // (or the operator, through the console or this control) put it there.
    //
    // The triage itself still runs, one layer down: `handle_operator_message`
    // reads it to narrow the model's board tools on a question and to take the
    // cheap chat-only path on a greeting. It just never mints anything.
    //
    // NOT on a workflow copilot thread (issue #416): a copilot conversation is
    // ABOUT one graph, so a message there is never a request to the company to
    // build one — the confinement in the harness stops the turn from acting,
    // and this stops the route from acting on its behalf.
    let workflow_requested =
        !confined && message.deliverable == Some(crate::ports::types::MessageIntent::Workflow);
    let lexical = workflow_requested
        .then(|| crate::company::task_intent::to_title(message.text.trim()))
        .filter(|title| !title.trim().is_empty());
    if let Some(lexical) = lexical {
        let title = crate::ports::tasks::mint_task_title(
            message.text.trim(),
            Some(&lexical),
            runtime.titler(),
        )
        .await;
        // The full ask, kept as the note whenever the headline is not already
        // the whole of it — which a named title almost always is not. This is
        // where the context, the caveats and the operator's own wording live now
        // that the title is a name rather than an excerpt.
        let note = (title.as_str() != message.text.trim()).then(|| message.text.clone());
        // Issue #576: the prompt box opens the card **already in Planning**, so
        // the spine epic #183 draws — prompt in, deliverable out — runs without
        // a human dragging the first step. The card is created *directly* in
        // `planning` rather than created in `todo` and then promoted, and that
        // is the whole of the "fires exactly once" property:
        // `task_enters_planning` compares the previous column to the next, and
        // a card that does not exist yet has no previous column, so the single
        // `upsert_task` below is one transition into Planning and therefore one
        // pass. A create-then-promote would be two writes, two board events, and
        // a window in which the card is visible — and actionable — in To-do.
        //
        // **Only for a person.** `by` is `Some` only when a signed-in user is
        // behind this request; a machine credential resolves to `None`. An agent
        // that could open a self-promoting card would trigger a planning pass,
        // which can open further cards, which promote, which plan — a spend loop
        // with no human in it. The issue's own "a typo costs a planning call" is
        // about a *person's* mistake costing one call. Widening this later is
        // safe; narrowing it after a spend loop is not. `Agent` and `System` are
        // named explicitly rather than left to `None` so a future caller that
        // passes an agent actor is refused by this branch rather than by luck.
        let opened_by_a_person = matches!(
            by.as_ref().map(|actor| actor.kind),
            Some(crate::ports::types::ActorKind::User | crate::ports::types::ActorKind::Operator)
        );
        let column = if opened_by_a_person {
            crate::ports::tasks::COLUMN_PLANNING
        } else {
            crate::ports::tasks::COLUMN_TODO
        };
        // Issue #982: the card is handed to whoever the operator addressed, and
        // it is resolved HERE — before the single `upsert_task` below, which is
        // the write that fires the planning pass. That ordering is the whole of
        // the fix. A card born blank is a card the planning pass is entitled to
        // fill in from a content match of its title against teammate roles, and
        // that guess is what a DM to a named teammate was losing to; patching
        // the assignee on afterwards would not fix it, it would race it, and
        // cost a second board event besides.
        //
        // Best-effort in exactly one direction: every case that does not resolve
        // to a real teammate or desk degrades to `""`, which is what this site
        // wrote unconditionally before. A chat must never 400 and must never
        // lose its card over who it was addressed to.
        let assignee = addressed_assignee(&runtime, message.chat.as_deref()).await;
        let record = crate::ports::tasks::TaskRecord {
            id: crate::ports::generate_id(),
            title,
            note,
            column: column.to_string(),
            priority: "medium".to_string(),
            assignee,
            updated_at_millis: crate::ports::now_millis(),
            // Issue #982 + #1890 B, reconciled with D: the conversation this
            // card was opened from, so the settle marker lands back where the
            // work was asked for rather than only on the board. `relay_reply`
            // answers in it, and the console already renders a marker in a DM
            // channel — nothing there changes. `None` for an unaddressed
            // message, which is every card this site opened before.
            //
            // The thread half is **the same rule by which an answer to this
            // message threads**, which is why it is `reply_thread` and not
            // `thread_root()`. B alone read the message's own `parent`, so a
            // card raised from a channel-level question recorded no thread.
            // That was right while a thread was only ever something an operator
            // opened by hand. D changed what a thread IS: an answer now parents
            // to the message that opened the exchange, so that question is a
            // root, and a card raised from it belongs to the thread it just
            // started.
            //
            // Left as `thread_root()`, the two disagreed about one message: the
            // answer landed in a thread and the card's settle marker landed
            // flat in the channel — the conversation and its outcome in
            // different places, which is the failure B exists to prevent,
            // reintroduced by D moving the ground under it.
            //
            // Found by hand-testing B and D together. Neither suite could catch
            // it: B's has no auto-threading and D's has no cards. Step 5 is why
            // it cannot come back: the desk and the thread are one value now,
            // built by one constructor, so there is no second field to forget.
            origin: crate::ports::TaskOrigin::new(
                message.chat.clone(),
                reply_thread(accepted.thread_root(), accepted.message_seq),
            ),
            parent_task_id: None,
            // Nothing has run yet, so there is no deliverable to point at
            // (issue #339). The first successful settle stamps it.
            output: None,
            plan: None,
            // Issue #580: carry the operator's explicit once-vs-workflow choice
            // from the chat payload onto the card. Absent means `once`, so a
            // plain "do X" chat request opens a one-off card exactly as before;
            // "build me a workflow for X" (deliverable: "workflow") routes the
            // card through the builder pass when it reaches In Progress. Nothing
            // here infers the choice from the text (decision D2a).
            planning_attempts: Vec::new(),
            deliverable: message
                .deliverable
                .and_then(crate::ports::types::MessageIntent::deliverable)
                .unwrap_or_default(),
            workflow_proposal: None,
            // Issue #983: the turn that opened it. A card raised from chat used
            // to be the *only* visible sign that a long turn was under way, and
            // it had nothing pointing back at the turn — so an operator looking
            // at a card in Planning could not reach the attempt working it, and
            // a turn that opened a card was indistinguishable from one that
            // opened none. `origin_workflow_id` stays `None`: there is no graph
            // behind a chat turn, and inventing one would be a lie the board
            // then carries forever.
            origin_run_id: accepted.turn_id.clone(),
            // The message this card was opened for. The runtime turn that
            // follows finds the card by this and nothing else, so the headline
            // above is free to be a name rather than an excerpt.
            origin_message_seq: Some(accepted.message_seq),
            origin_workflow_id: None,
            bounced: None,
        };
        if let Err(err) = runtime.upsert_task(&record).await {
            tracing::warn!(error = %err, "failed to open task card for chat request");
            // CHAT-021: a card-open failure used to end here — logged
            // server-side, and the chat turn otherwise proceeded to a normal
            // 200. To the operator, the message they had just asked to be
            // tracked simply never became a card, with no word anywhere in
            // the product that it had tried and failed. Same shape and author
            // as the turn-failure notice above: a direct `AgentReply` in the
            // same desk this card would have opened in, so it round-trips
            // through history like any other reply.
            let notice = CompanyEvent::AgentReply {
                audience: Vec::new(),
                parent: reply_thread(accepted.thread_root(), accepted.message_seq),
                chat_id: message
                    .chat
                    .clone()
                    .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string()),
                agent_id: crate::ports::SYSTEM_AUTHOR.to_string(),
                text: "This should have opened a task card, but the card could not be saved. \
                       Nothing else was lost — send the message again, or open the card by hand."
                    .to_string(),
                steps: Vec::new(),
                task_id: None,
                outputs: Vec::new(),
                mentions: Vec::new(),
                mention_depth: 0,
            };
            if let Err(journal_err) = runtime.events().append(runtime.id(), notice).await {
                tracing::warn!(
                    error = %journal_err,
                    "failed to journal the card-open failure notice itself"
                );
            }
        }
    }
    // Issue #983: the message is already in the journal — `accept_chat_turn`
    // appended it when the request was accepted, which is the whole point, so
    // `chat/history` is right from that instant rather than from whenever this
    // cycle wins the per-company serial lock. The cycle is handed the seq it
    // landed under and skips the append; everything downstream, `input_seqs` and
    // the response's `messageId` included, is keyed on that same seq.
    let report = runtime
        .run_journaled_cycle(
            vec![(accepted.message_seq, accepted.message_event.clone())],
            accepted.turn_id.clone(),
        )
        .await?;
    Ok((report, feedback_note))
}

/// What accepting a chat turn produced, before any of the turn's work runs
/// (issue #983).
///
/// The three facts that have to exist the moment a request is accepted, rather
/// than whenever the turn eventually gets the lock: the operator's message is in
/// the transcript, a durable row says a turn is owed, and the journal carries a
/// line saying the company took the work on.
struct AcceptedTurn {
    /// The seq the operator's message was appended under. The turn's own
    /// `messageId`, and what the pre-journaled cycle is keyed on.
    message_seq: EventSeq,
    /// The event itself, so the cycle can hand the brain what was journaled
    /// rather than a reconstruction of it.
    message_event: CompanyEvent,
    /// The turn's durable row, when one could be minted. `None` means the run
    /// store refused — the turn still runs, untracked, because record-keeping
    /// does not get to fail the work it records.
    turn_id: Option<String>,
}

impl AcceptedTurn {
    /// The thread this turn was typed in (issue #1890 B) — `None` is the
    /// channel-level conversation.
    ///
    /// Read off the **journaled event**, not off the request body, for the same
    /// reason [`run_chat`] takes this type rather than a loose parent: the body
    /// names a parent by id as a string and this is the parsed, validated fact
    /// the append actually recorded. Two readings of one thread root is how the
    /// board and the transcript drift.
    ///
    /// A message's own `parent` IS its root — a reply is parented to its
    /// question's parent, never to the question — so there is no chain to walk.
    fn thread_root(&self) -> Option<EventSeq> {
        match &self.message_event {
            CompanyEvent::OperatorMessage { parent, .. } => *parent,
            // Unreachable: `accept_chat_turn` journals an `OperatorMessage` and
            // nothing else. An arm rather than an `unwrap`, because the honest
            // answer for any other event is "no thread", not a panic on a path
            // that owes the operator a reply.
            _ => None,
        }
    }
}

/// Journals an operator message and mints the turn owed for it (issue #983).
///
/// # Everything that can refuse, refuses first
///
/// `ensure_running` (a lifecycle an operator chose — paused, archived) and
/// `ensure_accepting` (a runtime being replaced) are both checked **before** the
/// append. Ordered the other way, a refused request would still leave the
/// operator's question in the transcript with nothing that will ever answer it —
/// which is worse than the pre-#983 behaviour, not better, because a message
/// that is visibly there and permanently unanswered reads as lost work.
///
/// # The row is `Pending`, deliberately
///
/// `create_run` here, `begin_run` inside the cycle once it actually holds the
/// serial lock. So `Pending` means "queued behind other turns" — the serial
/// train five concurrent messages produce — and `Running` means "owns the lock".
/// Starting the row here would collapse the two and hide exactly the wait an
/// operator on a busy company is trying to understand.
///
/// The row is a [`RunRecord`](crate::ports::runs::RunRecord) rather than a store
/// of its own, which is what makes this small: it inherits transition legality,
/// the step trace, `list_stale_active`, the boot reaper — whose boot-only proof
/// holds verbatim for a chat turn, since a turn is a process-local
/// `tokio::spawn` serialising on the same per-company mutex — and the
/// `GET {scope}/runs` / `GET {scope}/runs/{run_id}` routes that already exist.
/// There is no new route here, and no new poll endpoint to design.
///
/// Best-effort on the row and on the transcript line, never on the append: the
/// message is the thing the operator can lose, and the other two are how we
/// describe it.
/// Resolves the client's attachment `node_id`s to durable [`Attachment`]s
/// (issue #1682).
///
/// The whole security posture of chat attachments lives here. The client hands
/// this route ids only; every name / mime / size on the journaled event is read
/// from the company's own workspace tree, never from the request — so a client
/// cannot claim a `report.pdf` is a `photo.png`, nor pretend a two-byte file is
/// two gigabytes. Each id must resolve to a **binary** node in *this* company's
/// tree: a foreign id (the IDOR a shared, guessable ULID would otherwise open),
/// one that names a prose note, or one that names nothing is a `400`, on the
/// same terms a bad thread `parent` is. The tree scan is the same read
/// `upload()` does to re-fetch a just-stored node, so no new store surface is
/// introduced.
///
/// Preserves the caller's order and refuses on the first bad id, so the message
/// is never journaled with a partial or reordered attachment list.
///
/// Also reads and extracts each attachment's text where the format and size
/// allow it (issue #1682, codex review finding) — see
/// [`extracted_attachment_text`]. A sequential loop rather than
/// `node_ids.iter().map(..).collect()`: extraction reads bytes and must
/// `.await`, and a chat message carries at most a small handful of
/// attachments, so there is no throughput this would meaningfully cost.
async fn resolve_attachments(
    runtime: &Arc<CompanyRuntime>,
    id: &CompanyId,
    node_ids: &[String],
) -> Result<Vec<Attachment>, ApiError> {
    if node_ids.is_empty() {
        return Ok(Vec::new());
    }
    // Codex review finding: an unbounded, unduplicated list turns one `/chat`
    // POST into an attacker-controlled multiplier on the extraction work
    // below — each id, however many times it repeats, is a tree scan plus up
    // to `MAX_ATTACHMENT_EXTRACT_BYTES` of reads and a parse. Refused before
    // either cost is paid, on the same terms a malformed `parent` is.
    if node_ids.len() > MAX_CHAT_ATTACHMENTS {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "a message may carry at most {MAX_CHAT_ATTACHMENTS} attachments, got {}",
            node_ids.len()
        ))));
    }
    // Deduplicated, order preserved: attaching the same file twice to one
    // message is never a meaningful distinct attachment, so a repeated id
    // resolves — and, more to the point, extracts — exactly once rather than
    // once per repetition.
    let mut seen = std::collections::HashSet::with_capacity(node_ids.len());
    let node_ids: Vec<&String> = node_ids.iter().filter(|id| seen.insert(*id)).collect();
    let tree = runtime.workspace().tree(id).await?;
    let mut resolved = Vec::with_capacity(node_ids.len());
    for node_id in node_ids {
        let node = tree.iter().find(|n| &n.id == node_id).ok_or_else(|| {
            ApiError(OpenCompanyError::InvalidRequest(format!(
                "attachment {node_id} is not in this company's workspace"
            )))
        })?;
        if node.kind != crate::ports::workspace::NodeKind::File {
            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                "attachment {node_id} is a folder, not a file"
            ))));
        }
        let (mime, size, extracted_text) = if node.is_binary() {
            (
                node.mime.clone().unwrap_or_default(),
                node.size.unwrap_or(0),
                extracted_attachment_text(runtime, id, node).await,
            )
        } else {
            let (content, size) = note_within_extract_cap(runtime, id, &node.id).await;
            (
                mime_guess::from_path(&node.name)
                    .first_raw()
                    .unwrap_or("text/plain")
                    .to_string(),
                size,
                extracted_note_text(&content),
            )
        };
        resolved.push(Attachment {
            node_id: node.id.clone(),
            name: node.name.clone(),
            mime,
            size,
            extracted_text,
        });
    }
    Ok(resolved)
}

/// The most attachments one chat message may carry (codex review finding).
///
/// The composer stages one file at a time (v1), so this is nowhere near the
/// operator's own path — it exists to bound what an unbounded client
/// request could otherwise force `resolve_attachments` to do: a tree scan
/// and an extraction pass per id, and extraction is not free
/// ([`MAX_ATTACHMENT_EXTRACT_BYTES`] of reads and a parse). Generous enough
/// for the multi-file UI the wire shape (`Vec<Attachment>`) already allows
/// room for, small enough that even the worst case — every id resolving and
/// maxing out the extraction cap — stays bounded per request.
const MAX_CHAT_ATTACHMENTS: usize = 20;

/// The largest attachment [`resolve_attachments`] reads for extraction, in
/// bytes.
///
/// Well below [`crate::ingest::MAX_DOCUMENT_BYTES`] on purpose — that cap is
/// for the dedicated memory-drop page, where reading a large document is the
/// whole point of the request. A chat attachment's extraction instead runs
/// inline in the synchronous `/chat` POST, so it stays small enough that an
/// otherwise-instant send never feels stuck parsing a PDF.
const MAX_ATTACHMENT_EXTRACT_BYTES: u64 = 4 * 1024 * 1024;

/// The most extracted text one attachment contributes to the wire, in chars.
///
/// [`crate::brain::medulla::wire::WireEvent::body`] caps at 200000 chars and
/// carries the operator's own words too, so no single attachment may be free
/// to crowd out the rest of the turn.
const MAX_ATTACHMENT_EXTRACT_CHARS: usize = 6_000;

/// One prose node's byte length, and its body only while that length stays
/// within [`MAX_ATTACHMENT_EXTRACT_BYTES`].
///
/// [`WorkspaceStore::read_capped`](crate::ports::workspace::WorkspaceStore::read_capped)
/// rather than a read and a length check, so the ceiling holds where the binary
/// path's does — before the transfer, not after it. A plain `read` would
/// materialise the whole note to discover it must be discarded, and a message
/// may carry [`MAX_CHAT_ATTACHMENTS`] of them.
///
/// Best-effort on the same terms as [`extracted_attachment_text`]: a read that
/// races a delete or hits a transient store error leaves the reference itself
/// intact rather than failing the send. The size is then `0`, which is what the
/// caller can honestly say about a body it could not measure.
async fn note_within_extract_cap(
    runtime: &Arc<CompanyRuntime>,
    id: &CompanyId,
    node_id: &str,
) -> (String, u64) {
    runtime
        .workspace()
        .read_capped(id, node_id, MAX_ATTACHMENT_EXTRACT_BYTES)
        .await
        .ok()
        .flatten()
        .map(|(_, body, len)| (body, len))
        .unwrap_or_default()
}

/// A prose attachment's text for the brain, `None` when there is none to carry
/// — an empty note, or one the store withheld for weighing more than the
/// extraction cap.
fn extracted_note_text(content: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    Some(crate::ledger::budget::truncate(
        content,
        MAX_ATTACHMENT_EXTRACT_CHARS,
    ))
}

/// Reads and extracts one binary node's text where the format and size allow
/// it, `None` otherwise (issue #1682, codex review finding).
///
/// `None` covers three cases alike — an image or other format nothing here
/// parses, a scan with no text layer, and a payload over
/// [`MAX_ATTACHMENT_EXTRACT_BYTES`] — because for "does the brain have
/// something to read" a caller does not need to tell them apart. Reuses
/// [`crate::ingest::extract`], the same PDF/DOCX/PPTX/XLSX/plain-text
/// pipeline the memory-drop page already runs, so a chat attachment's actual
/// words ride the durable [`Attachment`] rather than leaving a hosted or
/// sidecar brain with only a node id and no device tool that resolves it.
///
/// Best-effort: any read failure (a race with a delete, a transient store
/// error) answers `None` rather than failing the send — the reference alone
/// still reaches the transcript and the journal.
async fn extracted_attachment_text(
    runtime: &Arc<CompanyRuntime>,
    id: &CompanyId,
    node: &crate::ports::workspace::WorkspaceNode,
) -> Option<String> {
    let size = node.size?;
    if size == 0 || size > MAX_ATTACHMENT_EXTRACT_BYTES {
        return None;
    }
    let (_, stream) = runtime
        .workspace()
        .read_bytes(id, &node.id)
        .await
        .ok()
        .flatten()?;
    let bytes = drain_bounded(stream, MAX_ATTACHMENT_EXTRACT_BYTES).await?;
    // The extraction pipeline is synchronous CPU work — PDF/DOCX/PPTX/XLSX
    // parsing — that can run for a while on a document near the size cap, and
    // this runs inline in the `/chat` POST. Dispatch it to the blocking pool
    // rather than stalling a Tokio worker (codex review finding). The owned
    // pieces are cloned out of the borrowed node first: `spawn_blocking`
    // requires its closure's captures to be `'static`.
    let name = node.name.clone();
    let mime = node.mime.clone();
    tokio::task::spawn_blocking(move || {
        match crate::ingest::extract(&name, mime.as_deref(), &bytes) {
            crate::ingest::Extracted::Text(text) => Some(crate::ledger::budget::truncate(
                &text,
                MAX_ATTACHMENT_EXTRACT_CHARS,
            )),
            crate::ingest::Extracted::Empty | crate::ingest::Extracted::Unsupported(_) => None,
        }
    })
    .await
    .ok()
    .flatten()
}

/// Drains a [`BlobStream`](crate::ports::workspace::BlobStream) into a
/// buffer, `None` if it ever exceeds `cap` or errors partway through (codex
/// review finding).
///
/// Split out from [`extracted_attachment_text`] so the one property that
/// matters here — a stream error discards what was read, rather than handing
/// extraction a truncated payload that looks complete — is directly testable
/// against a synthetic stream, without a real workspace store behind it.
///
/// A stream error mid-read used to fall straight through to extraction on
/// whatever partial bytes had been collected: `while let Ok(Some(chunk)) =
/// stream.try_next().await` cannot tell "the stream ended" from "the stream
/// errored", so it just stopped accumulating either way. A truncated payload
/// is not a smaller version of the file; it can parse into plausible-looking
/// but wrong or incomplete text (a document missing its ending, a multi-byte
/// sequence cut mid-codepoint) with nothing marking it as partial once it
/// reaches the brain. "No readable text" is honest; a guess dressed as a
/// read is not.
async fn drain_bounded(
    mut stream: crate::ports::workspace::BlobStream,
    cap: u64,
) -> Option<Vec<u8>> {
    use futures::TryStreamExt;

    let mut bytes = Vec::new();
    loop {
        match stream.try_next().await {
            Ok(Some(chunk)) => {
                bytes.extend_from_slice(&chunk);
                // Belt-and-braces against a store whose streamed length
                // disagrees with the metadata length its caller expected —
                // never buffer past the cap just because the node claimed to
                // be under it.
                if bytes.len() as u64 > cap {
                    return None;
                }
            }
            Ok(None) => return Some(bytes),
            Err(_) => return None,
        }
    }
}

/// Who a chat turn addressed to `desk` is expected to be answered by.
///
/// Recorded on the turn's row so a console with no receipt can still name the
/// teammate. A chat turn used to record the DESK here — `for_chat(.., desk,
/// desk)` — so every one of them read `agent_id == chat_id` ("main" for
/// General). The console's rich receipt names whoever the first live frame
/// named, but a receipt is client state: a reload throws it away, re-arms from
/// `/runs` alone, and could then render only a bare "Working…" — no name, no
/// clock, nothing to distinguish a turn in flight from a console that lost it.
///
/// The same ladder [`crate::runtime::cycle`]'s small-talk fast path runs, which
/// is itself "the same resolution the harness brain's `responder_for` runs", so
/// the name on the row is the voice the turn will actually answer in.
///
/// **Optimistic, and the callers depend on it being cheap.** This is the sync,
/// record-only seam — no inference — so the brain's per-message rung may still
/// pick a different seat; the turn's first live frame supersedes whatever is
/// recorded here. A company with nobody resolvable falls back to `desk`, which
/// is precisely the old behaviour, so the change can only add a name.
fn chat_turn_responder(record: &crate::ports::types::CompanyRecord, desk: &str) -> String {
    crate::runtime::delegation_tools::chat_responder(record, desk)
        .or_else(|| crate::company::orchestrator_id(&record.effective_agents()).map(str::to_string))
        .unwrap_or_else(|| desk.to_string())
}

async fn accept_chat_turn(
    runtime: &Arc<CompanyRuntime>,
    id: &CompanyId,
    message: &ChatMessage,
    by: Option<&Actor>,
    parent: Option<EventSeq>,
    desk: &str,
) -> Result<AcceptedTurn, ApiError> {
    runtime.ensure_running().await?;
    runtime.ensure_accepting().map_err(ApiError)?;

    // Issue #1682: resolve the client's attachment ids to durable references
    // before the journal write, so a bad reference refuses the send outright —
    // on the same terms a malformed `parent` does — rather than journaling a
    // message that points at a file this company does not have.
    let attachments = resolve_attachments(runtime, id, &message.attachments).await?;

    // Both halves of one resolution: who this message reached, and every
    // `@name` that reached more than one thing and therefore reached nobody
    // (B-101). The second half is reported below, after the message is
    // journaled, so the notice can never precede the line it is about.
    let resolved = runtime
        .resolve_mentions_reporting(&message.text, message.mentions.clone(), by)
        .await;

    let message_event = CompanyEvent::OperatorMessage {
        text: message.text.clone(),
        by: by.cloned(),
        // Thread the addressed desk through so the orchestrator brain can
        // route to that desk's lead member (issue #53).
        chat: message.chat.clone(),
        // …and the message being replied to, so the thread is a fact about
        // the transcript rather than about one browser (issue #364).
        parent,
        // Issue #845: and the once-vs-workflow choice, so the turn that
        // answers this message knows whether the builder pass owns the
        // authoring. Without it the turn ran blind and denied a capability
        // that was being exercised on the very same message — see the field
        // docs on `CompanyEvent::OperatorMessage`.
        deliverable: message.deliverable,
        // Resolved before the journal write, so the durable record and the
        // routing decision that follows read the same list. The picker's answer
        // when it sent one, extraction from the text when it did not — and
        // either way re-validated against the live roster.
        mentions: resolved.mentions.clone(),
        // Issue #1682: the store-resolved references, so the durable record
        // carries the name/mime/size the store computed and never the client's
        // claim. Empty on a message with no attachment, which skips the field.
        attachments,
    };
    // Asked again, immediately before the durable write. The check above sits
    // two awaits back — attachment and mention resolution both yield — and a
    // stop landing in that window would leave a message in the transcript that
    // no turn will ever answer, on a company that has already reported itself
    // stopped. The first check is still worth keeping: it refuses before the
    // resolution work rather than after it.
    runtime.ensure_accepting().map_err(ApiError)?;
    let message_seq = runtime
        .events()
        .append(id, message_event.clone())
        .await
        .map_err(ApiError)?;

    // The durable half of a mention (issue: mentions).
    //
    // The SSE feed only reaches a browser that is open, so without this a
    // mention is invisible to everyone who was not watching when it landed —
    // which is most of the point of mentioning somebody. Filed here, right
    // after the journal write, so the notification and the message share a
    // sequence and a turn that later fails still leaves the mention recorded.
    //
    // Deliberately not fatal: a notification store that will not answer must
    // not fail somebody's message. The mention still renders as a chip and is
    // still in the transcript; only the badge is missing, and the warning says
    // so.
    if let CompanyEvent::OperatorMessage { mentions, .. } = &message_event
        && !mentions.is_empty()
    {
        runtime
            .notify_mentions(id, mentions, &message_seq, by, desk)
            .await;
    }

    // The other half of the same resolution (B-101): every `@name` that reached
    // two things and therefore reached nobody. Posted after the message's own
    // append so the notice can never sort above the line it is about, and on
    // the same not-fatal terms as the notifications above — a message whose
    // advisory could not be written is still a delivered message.
    runtime
        .post_mention_ambiguity_note(desk, parent, &resolved.ambiguous)
        .await;

    let turn_id = crate::ports::generate_id();
    // A record this read cannot load leaves the turn recorded exactly as it was
    // before: the desk's own id, which is what `for_chat` was passed twice.
    let responder = match runtime.store().load(id).await {
        Ok(Some(record)) => chat_turn_responder(&record, desk),
        _ => desk.to_string(),
    };
    let turn_id = match runtime
        .runs()
        .create_run(
            id,
            // Which *thread* this turn is in, not just which channel. A
            // channel holds many threads since #1890 and `chat_id` names only
            // the channel, so without this the console cannot tell whose turn
            // is running and suppresses the working indicator for the whole
            // channel whenever any thread is open — hiding a turn the host is
            // actively running.
            //
            // Only a threaded reply carries a root. A message sent from the
            // channel composer is left unrooted deliberately: its turn is the
            // channel's own, it is what the channel timeline shows, and the
            // console has to arm its indicator optimistically at POST time —
            // before the host has assigned this message a seq. Rooting it at
            // its own seq would key the two legs differently and the reload
            // leg would stop matching the arm.
            crate::ports::runs::NewRun::for_chat(turn_id.clone(), desk, responder)
                .in_thread(parent),
        )
        .await
    {
        Ok(run) => Some(run.id),
        Err(err) => {
            tracing::warn!(
                company = %id,
                turn = %turn_id,
                error = %err,
                "[runs] could not open a turn row; the turn runs untracked"
            );
            None
        }
    };

    // The transcript line. Separate from the row on purpose: the row answers
    // "what is the status", and this answers "was a turn accepted for this
    // message at all" — which the log cannot otherwise say, because an
    // `OperatorMessage` with no reply after it is indistinguishable from a
    // chatter message that legitimately produced none.
    if let Some(turn_id) = turn_id.clone()
        && let Err(err) = runtime
            .events()
            .append(
                id,
                CompanyEvent::TurnStarted {
                    turn_id,
                    chat_id: desk.to_string(),
                    parent,
                    by: by.cloned(),
                },
            )
            .await
    {
        tracing::warn!(
            company = %id,
            error = %err,
            "could not journal a turn's acceptance; its row still records it"
        );
    }

    Ok(AcceptedTurn {
        message_seq,
        message_event,
        turn_id,
    })
}

/// Settles a chat turn's durable row, and says so in the transcript when it
/// failed (issue #983).
///
/// Runs inside the spawned turn, beside the reply journaling and for the same
/// reason: a client that walked away must not take the record with it. A turn
/// whose row is left active is not silently forgiven either — the boot reaper
/// fails it on the next start, on exactly the proof it uses for a dispatch.
async fn settle_chat_turn(
    runtime: &Arc<CompanyRuntime>,
    id: &CompanyId,
    turn_id: Option<&str>,
    failure: Option<&ApiError>,
) {
    let Some(turn_id) = turn_id else { return };
    let outcome = match failure {
        None => crate::ports::runs::RunOutcome::new(crate::ports::runs::RunStatus::Succeeded),
        Some(err) => crate::ports::runs::RunOutcome::new(crate::ports::runs::RunStatus::Failed)
            .with_error(err.0.to_string()),
    };
    if let Err(err) = runtime.runs().finish_run(id, turn_id, outcome).await {
        tracing::warn!(
            company = %id,
            turn = %turn_id,
            error = %err,
            "[runs] could not settle a turn row; the next boot reaps it"
        );
    }
    // Only a failure gets a transcript line. A turn that answered has an
    // `AgentReply` right there saying so, and a second "it finished" line would
    // be one more thing to read for no information.
    if let Some(failure) = failure
        && let Err(err) = runtime
            .events()
            .append(
                id,
                CompanyEvent::TurnFailed {
                    turn_id: turn_id.to_string(),
                    error: failure.0.to_string(),
                },
            )
            .await
    {
        tracing::warn!(
            company = %id,
            turn = %turn_id,
            error = %err,
            "could not journal a turn's failure; its row still records it"
        );
    }
}

/// Runs a chat cycle and emits any implied webhooks, rendering the responses.
async fn chat_and_emit(
    state: &AppState,
    id: &CompanyId,
    runtime: Arc<CompanyRuntime>,
    message: ChatMessage,
    by: Option<Actor>,
) -> Result<ChatOk, ApiError> {
    // The default desk for an unaddressed message.
    let desk = message
        .chat
        .clone()
        .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string());
    // Issue #1757: the Operator channel is a **read-only** aggregation surface —
    // a "what happened" feed of workflow reports, not a conversation. Refuse a
    // send addressed to it rather than journaling an `OperatorMessage` under the
    // `operator` line (which would both make it writable and mix chatter into the
    // report feed). The frontend hides its send box; this is the safety net.
    //
    // The check (migration carve-outs, error text) lives on `CompanyRuntime`
    // itself now — `ensure_desk_writable` — so the ACP `session/prompt` route
    // (issue #1781 review, Codex P1), which journals straight to
    // `runtime.events()` without ever calling this function, runs the exact
    // same guard rather than a second hand-copied one that could drift.
    runtime.ensure_desk_writable(&desk).await?;
    // Issue #364: a thread reply names its parent by id. Rejected here rather
    // than dropped, so a console sending a malformed parent learns that its
    // reply would have landed in the channel instead of quietly finding it
    // there later.
    let parent = match message.parent.as_deref() {
        Some(raw) => Some(parse_message_id(raw)?),
        None => None,
    };
    // A reply to a settled `in_review` dispatch card's settle pill or relay
    // bubble is review feedback, not a fresh turn. It is appended to the card
    // and re-runs it through the dispatch choke point; the re-run journals its
    // own relay on settle. Only a threaded message can be review feedback, so a
    // top-level line never reaches here.
    #[cfg(feature = "openhuman")]
    if let Some(parent) = parent {
        let _serialized = runtime.task_writes.lock().await;
        if let Some(card) = runtime.review_feedback_target(&desk, parent).await? {
            let accepted =
                accept_chat_turn(&runtime, id, &message, by.as_ref(), Some(parent), &desk).await?;
            let message_id = accepted.message_seq.value().to_string();
            let turn_id = accepted.turn_id.clone();
            let review = runtime
                .apply_review_feedback(&card, &message.text, by.as_ref())
                .await
                .map_err(ApiError);
            settle_chat_turn(&runtime, id, turn_id.as_deref(), review.as_ref().err()).await;
            review?;
            return Ok(ChatOk::Settled(Box::new(ChatResponse {
                responses: Vec::new(),
                message_id: Some(message_id),
                still_awaiting: None,
                turn_id,
                outcome: None,
                review_feedback_applied: Some(true),
                settled_ids: None,
            })));
        }
    }
    // Issue #1862: a reply that answers a parked blocker settles its verdict
    // rather than running a fresh turn. A reply parented to a blocker card
    // resolves that card's group; free text in a DM that holds a single blocked
    // thing resolves it; free text where several are blocked asks which. Runs
    // after the review check above — the two anchor on different event kinds, so
    // neither steals the other's replies — and only reaches here when the reply
    // is a verdict for a blocker actually pending in this conversation;
    // otherwise it falls through to the ordinary turn.
    #[cfg(feature = "openhuman")]
    {
        // The guard covers the read-and-classify only, and is released before
        // anything is settled. `apply_blocker_reply` waits on a follow-up that
        // runs on a spawned task and takes `task_writes` for the board edit its
        // resume makes, so holding the lock across it would wait forever on a
        // task that is waiting for this lock. Nothing between the two needs it:
        // `accept_chat_turn` journals the message and touches no board.
        let plan = {
            let _serialized = runtime.task_writes.lock().await;
            runtime
                .plan_blocker_reply(&desk, parent, &message.text)
                .await?
        };
        match plan {
            crate::company::runtime::BlockerReplyPlan::Resolve { ids, intent } => {
                let accepted =
                    accept_chat_turn(&runtime, id, &message, by.as_ref(), parent, &desk).await?;
                let message_id = accepted.message_seq.value().to_string();
                let turn_id = accepted.turn_id.clone();
                let applied = runtime
                    .apply_blocker_reply(&ids, intent, &message.text, by.as_ref())
                    .await
                    .map_err(ApiError);
                settle_chat_turn(&runtime, id, turn_id.as_deref(), applied.as_ref().err()).await;
                applied?;
                return Ok(ChatOk::Settled(Box::new(ChatResponse {
                    responses: Vec::new(),
                    message_id: Some(message_id),
                    still_awaiting: None,
                    turn_id,
                    outcome: None,
                    review_feedback_applied: Some(true),
                    settled_ids: None,
                })));
            }
            crate::company::runtime::BlockerReplyPlan::AskWhich { prompt } => {
                let accepted =
                    accept_chat_turn(&runtime, id, &message, by.as_ref(), parent, &desk).await?;
                let message_id = accepted.message_seq.value().to_string();
                let turn_id = accepted.turn_id.clone();
                let posted = runtime
                    .post_blocker_prompt(
                        &desk,
                        reply_thread(accepted.thread_root(), accepted.message_seq),
                        &prompt,
                    )
                    .await
                    .map_err(ApiError);
                settle_chat_turn(&runtime, id, turn_id.as_deref(), posted.as_ref().err()).await;
                posted?;
                return Ok(ChatOk::Settled(Box::new(ChatResponse {
                    responses: Vec::new(),
                    message_id: Some(message_id),
                    still_awaiting: None,
                    turn_id,
                    outcome: None,
                    review_feedback_applied: Some(true),
                    settled_ids: None,
                })));
            }
            crate::company::runtime::BlockerReplyPlan::NotBlocker => {}
        }
    }
    // The turn runs on its own task, and the replies are journaled there too
    // (issue #882). Both used to sit in this handler's future, which hyper drops
    // the moment the peer goes away — and a reverse proxy in front of a hosted
    // tenant goes away the moment it decides the upstream is too slow. A turn
    // slower than that timeout was therefore cancelled mid-flight: tokens spent,
    // side effects half-applied, and no `AgentReply` ever appended, so the
    // operator's DM history held their question and no answer and the turn could
    // neither be read back nor resumed.
    //
    // Awaiting the handle is drop-safe — dropping it abandons the *waiting*, not
    // the work — so this answers exactly as it did before and needs no wire
    // change to survive the disconnect. Same shape as the approval path
    // (`CompanyRuntime::resolve_approval_spawned`, issue #380 defect 3) and the
    // workflow runner (`WorkflowSpawn::spawn_admitted`), which is why a 504'd
    // workflow run kept executing while a 504'd chat turn did not.
    // Issue #983: the operator's message reaches the journal here, before the
    // turn is spawned and therefore before it queues on the per-company serial
    // lock. It used to be appended inside that lock, so five concurrent messages
    // became a serial train in which the fifth operator's question was invisible
    // — a reload showed an empty conversation — until the four ahead of it had
    // finished. A durable row and a transcript line are minted alongside it, so
    // a turn killed with the pod becomes a `Failed` row and a `TurnFailed` line
    // rather than permanent silence.
    let accepted = accept_chat_turn(&runtime, id, &message, by.as_ref(), parent, &desk).await?;
    // Read off the accepted turn before it moves onto the task: both are facts
    // the accept already established, so the 202 can carry them without waiting
    // for a cycle that has not even taken the lock yet.
    let turn_id = accepted.turn_id.clone();
    let message_id = accepted.message_seq.value().to_string();
    let detach = message.detach;
    let turn = spawn_chat_turn(ChatTurn {
        runtime,
        company: id.clone(),
        desk,
        message,
        by,
        parent,
        accepted,
    });

    if detach && let Some(turn_id) = turn_id.as_ref() {
        // Nothing here waits on the turn. The webhook fan-out still owes the
        // report, so it moves onto its own task rather than being dropped — a
        // detached turn must not silently stop notifying subscribers. Same shape
        // as the detached approval resolve below (issue #561).
        //
        // The turn task is otherwise left to itself: it journals its own replies
        // and settles its own row (issue #983), which is what the operator reads
        // back. Detaching is the entire point.
        //
        // The row is what the detached contract is built on: the console arms
        // its poll from this `202`'s `turnId` (issue #983), and that poll is
        // the only delivery path when `/events` is buffered or unavailable
        // (`opencompany-microservice#23`) — which is exactly the state #983
        // exists for. A `202` with no row would strand the reply until reload,
        // so a detach whose row the run store refused falls through to the
        // synchronous settle below instead: the console learns it never
        // detached, and the reply arrives in the body like any settled turn.
        let state = state.clone();
        let company = id.clone();
        tokio::spawn(async move {
            match join_chat_turn(turn).await {
                Ok((report, feedback_note)) => {
                    emit_cycle_webhooks(&state, &company, &report).await;
                    if let Some(note) = feedback_note {
                        emit_feedback_webhook(&state, &company, &note).await;
                    }
                }
                // A failed turn already settled its row as `Failed` and wrote a
                // `TurnFailed` transcript line, which is what the operator sees;
                // there is no report to fan out. Logged because nothing else
                // reports it once the request is gone.
                Err(err) => {
                    tracing::error!(%company, detail = %err.0, "[chat] a detached turn did not finish");
                }
            }
        });
        return Ok(ChatOk::Detached(DetachedChatResponse {
            turn_id: turn_id.clone(),
            message_id,
            detached: true,
        }));
    }

    let (report, feedback_note) = join_chat_turn(turn).await?;
    let responses = readable_responses(report.responses.clone());
    emit_cycle_webhooks(state, id, &report).await;
    if let Some(note) = feedback_note {
        emit_feedback_webhook(state, id, &note).await;
    }
    Ok(ChatOk::Settled(Box::new(ChatResponse {
        // The operator's own message is the cycle's single input event, so its
        // sequence is the first the cycle journaled (issue #364).
        message_id: report.input_seqs.first().map(|seq| seq.value().to_string()),
        responses,
        // A chat turn is nobody's sign-off, so this stays absent here.
        still_awaiting: None,
        turn_id,
        // …and it resolves nothing, so there is no resolve outcome to report.
        outcome: None,
        review_feedback_applied: None,
        settled_ids: None,
    })))
}

/// The HTTP status written immediately after `marker` in `lower`, when one is.
///
/// `lower` must already be lowercased. Only a three-digit run counts, so a
/// message that merely mentions the marker cannot produce a status.
///
/// Needed because our own errors read `inference returned 429 Too Many
/// Requests: …`, and `structured_http_status` looks for a status at the start
/// of the string, after a `(`, or behind an `http`/`status:` marker — none of
/// which that shape offers. The status we already knew was therefore invisible
/// to the classifier, leaving classification to whatever prose the provider
/// happened to choose.
fn status_after_marker(lower: &str, marker: &str) -> Option<u16> {
    let rest = lower.split_once(marker)?.1.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() != 3 {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
#[path = "operator_turn_failure_notice_tests.rs"]
mod turn_failure_notice_tests;

/// Keys rework #2306, round-2 review KR-L2-03: the structured turn-failure
/// fields (`userFacing`, `code`, `message`, `pairAgentId`, `providerSlug`),
/// end to end through `MessageView`/`ChatHistoryMessageDto` — the
/// `in-use-guards.md` §5 wire contract Agent C's console codes directly
/// against.
#[cfg(test)]
#[path = "operator_resolution_failure_wire_tests.rs"]
mod resolution_failure_wire_tests;

/// What an operator is told when a turn could not be finished.
///
/// The raw error is a diagnostic and never belongs in company chat. On the
/// rate-limit path it was a wall of provider JSON with a settings URL in it,
/// which is what a tester saw instead of an answer (issue #2016). It is logged
/// in full at the call site; this renders the one sentence that tells the
/// operator whether to wait, retry, or go and fix something.
///
/// Deliberately narrow about when it blames the provider. `classify_provider_
/// failure` falls back to `Retryable` for text it recognizes nothing in, so
/// classifying every failure would describe a tool that timed out as a provider
/// outage. A cause is named only when the error is one the inference path
/// actually emits; anything else keeps the generic wording.
fn turn_failure_notice(detail: &str) -> String {
    const CLOSING: &str = "Nothing was left half-done.";
    let cause = provider_failure_sentence(detail).unwrap_or(
        "This turn couldn't be finished — something went wrong or a step took too long.",
    );
    format!("{cause} {CLOSING} Send the message again to retry.")
}

/// The operator-facing sentence for a failure the inference path produced, or
/// `None` when the failure did not come from there.
///
/// Deliberately narrow about when it blames the provider. The classifier falls
/// back to `Retryable` for text it recognizes nothing in, so classifying every
/// failure indiscriminately would report a tool that ran out of wall-clock as a
/// provider outage. A cause is named only when the error is one the inference
/// path actually emits, or carries a recognizable HTTP status.
fn provider_failure_sentence(detail: &str) -> Option<&'static str> {
    let lower = detail.to_ascii_lowercase();

    // An empty turn is its own case, and not one more retrying fixes: the
    // harness has already retried it by the time this is written. Recognized
    // from our own error text, so it holds in every build.
    if lower.contains("carried neither") {
        return Some(
            "This turn couldn't be finished — the AI provider returned an empty response.",
        );
    }

    let status = status_after_marker(&lower, "inference returned ");
    let from_inference = lower.contains("inference returned")
        || lower.contains("inference request failed")
        || lower.contains("inference response")
        || lower.contains("configured inference model");
    if !from_inference && status.is_none() {
        return None;
    }

    classified_provider_sentence(status, detail)
}

/// The sentence for a recognized provider failure, classified through the
/// harness's own [`classify_provider_failure`].
///
/// Reused rather than re-implemented: the crate already knows which 429s are
/// transient and which mean an account needs topping up, and a second
/// classifier here would drift from the one that decides whether to retry.
///
/// [`classify_provider_failure`]: tinyagents_harness::retry::classify_provider_failure
#[cfg(feature = "openhuman")]
fn classified_provider_sentence(status: Option<u16>, detail: &str) -> Option<&'static str> {
    use tinyagents_harness::retry::{
        ProviderFailureClass, classify_provider_failure, structured_http_status,
    };

    let status = status.or_else(|| structured_http_status(detail));
    Some(match classify_provider_failure(status, None, detail) {
        ProviderFailureClass::RateLimited => {
            "This turn couldn't be finished — the AI provider is rate-limiting requests."
        }
        ProviderFailureClass::NonRetryableRateLimit => {
            "This turn couldn't be finished — the AI provider reports no quota or credit left. \
             An admin needs to check the provider account under Settings."
        }
        ProviderFailureClass::NonRetryable => {
            "This turn couldn't be finished — the AI provider rejected the request, usually a \
             model or configuration mismatch. An admin can check Settings."
        }
        ProviderFailureClass::UpstreamUnhealthy | ProviderFailureClass::Retryable => {
            "This turn couldn't be finished — the AI provider is temporarily unavailable."
        }
    })
}

/// The default build links no inference harness at all — it keeps the
/// echo-brained offline behaviour — so it produces no provider failures to
/// classify and has no classifier to reach for. The generic notice is the
/// honest answer there.
#[cfg(not(feature = "openhuman"))]
fn classified_provider_sentence(_status: Option<u16>, _detail: &str) -> Option<&'static str> {
    None
}

/// Everything a chat turn needs once it is off the request's future.
///
/// A struct rather than six positional arguments because the spawn boundary is
/// exactly where a mis-ordered pair of `String`s would compile and then journal
/// replies against the wrong desk.
struct ChatTurn {
    runtime: Arc<CompanyRuntime>,
    company: CompanyId,
    desk: String,
    message: ChatMessage,
    by: Option<Actor>,
    parent: Option<EventSeq>,
    /// What accepting the turn already wrote (issue #983): the journaled
    /// message, and the row this task owes a settle.
    accepted: AcceptedTurn,
}

/// Runs a chat turn and journals its replies on a task of its own (issue #882).
///
/// The journal write belongs on this side of the spawn, not back in the handler.
/// Spawning only the cycle would still lose the answer: the turn would finish,
/// and the `AgentReply` append that makes it readable — and that the `agent_reply`
/// SSE frame is derived from — would die with the dropped handler future. The
/// work is not recorded until it is journaled, so both halves move together.
fn spawn_chat_turn(turn: ChatTurn) -> JoinHandle<Result<(CycleReport, Option<String>), ApiError>> {
    tokio::spawn(async move {
        let ChatTurn {
            runtime,
            company,
            desk,
            message,
            by,
            parent,
            accepted,
        } = turn;
        let turn_id = accepted.turn_id.clone();
        // Issue #983: the settle lives on this side of the spawn for the same
        // reason the reply journaling does — a proxy that gave up must not leave
        // a row claiming to be live. Both outcomes settle: an error here is a
        // turn that was accepted and produced no answer, which is precisely the
        // state that used to be indistinguishable from silence.
        let outcome = run_chat(Arc::clone(&runtime), message, by, &accepted).await;
        let (mut report, feedback_note) = match outcome {
            Ok(both) => both,
            Err(err) => {
                // A turn that aborts — most often a tool call that exceeded its
                // wall-clock budget — was only ever logged server-side. To the
                // operator watching the thread, the teammate simply vanished
                // mid-answer with no word. Journal a visible system line in the
                // same desk thread the reply would have gone to, naming the
                // failure and what to do next. Same shape and author as the
                // continuation-failure notice (SYSTEM_AUTHOR): a direct
                // `AgentReply` so it round-trips through history like any other
                // reply, and is distinguishable on disk from a real teammate
                // bubble. `err.0` is the inner error (it carries `Display`);
                // the `ApiError` newtype does not.
                let detail = err.0.to_string();
                // Keys rework #2306, round-2 review KR-L2-03: when the abort
                // is a classified resolution failure (a broken pin, a broken
                // default, no key, no model chosen at all), the bare X9
                // sentence is written here UNWRAPPED — no "Nothing was left
                // half-done, send it again" — because that closing line is
                // actively wrong advice for this class: resending fixes
                // nothing until the named setting is fixed. Every other
                // failure keeps `turn_failure_notice`'s wrapped generic
                // wording exactly as before. `MessageView::project` and the
                // SSE builder re-classify this same text at read time — no
                // new field on this event, so none of `AgentReply`'s ~30
                // other construction sites need to change.
                let text = match crate::company::inference::copy::classify(&detail) {
                    Some(resolution) => resolution.message,
                    None => turn_failure_notice(&detail),
                };
                let notice = CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    // Issue #1890 D: threaded on exactly the terms a successful
                    // reply is. This notice IS the answer when there is no
                    // other one, and `reply_thread`'s whole argument is that
                    // `parent` must not be a function of what happened at run
                    // time — deciding it by race timing was the case it names,
                    // and deciding it by whether the model answered is the same
                    // mistake. Left as the raw `parent`, one operator message
                    // opened a thread when the turn worked and stayed flat when
                    // it did not, so two identical sends produced two different
                    // transcripts depending on the weather.
                    //
                    // Found by hand-testing the epic against a company whose
                    // provider refused every turn — which is exactly the state
                    // that makes this the ONLY reply an operator gets.
                    parent: reply_thread(parent, accepted.message_seq),
                    chat_id: desk.clone(),
                    agent_id: crate::ports::SYSTEM_AUTHOR.to_string(),
                    text,
                    steps: Vec::new(),
                    task_id: None,
                    outputs: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                };
                // The raw provider text is a diagnostic, not an operator
                // message: it is unbounded, provider-shaped, and on the rate-limit
                // path it carried a wall of JSON and a settings URL into company
                // chat. It stays here, in full (issue #2016).
                tracing::warn!(
                    company = %company,
                    desk = %desk,
                    detail = %err.0,
                    "a chat turn could not be finished"
                );
                if let Err(journal_err) = runtime.events().append(&company, notice).await {
                    tracing::warn!(
                        company = %company,
                        error = %journal_err,
                        "an aborted chat turn could not be reported to the operator"
                    );
                }
                settle_chat_turn(&runtime, &company, turn_id.as_deref(), Some(&err)).await;
                return Err(err);
            }
        };
        let reply_parent = reply_thread(parent, accepted.message_seq);
        journal_chat_replies(&runtime, &company, &desk, reply_parent, &mut report).await;
        // SPIKE (tinyhivemind P15): a committed reply may refer work to another
        // desk. AFTER journaling, never before — the referral is keyed on the
        // reply's own sequence, so it has to exist first.
        #[cfg(feature = "hivemind")]
        refer_committed_replies(&runtime, &company, &desk, &report, None, None, 0).await;
        settle_chat_turn(&runtime, &company, turn_id.as_deref(), None).await;
        Ok((report, feedback_note))
    })
}

/// Offer each committed agent reply to the referral decision (tinyhivemind P15).
///
/// The decision is pure and the queue is the only thing that acts, so this is
/// safe to run over every reply: a message that refers nobody costs one
/// in-memory decision and calls the queue zero times.
///
/// Policy is deliberately hard-coded here for the spike. In production it is an
/// operator setting — `ReferralPolicy::DEFAULT` has every knob off, and that is
// the shipping default the library intends.
#[cfg(feature = "hivemind")]
pub(crate) async fn refer_committed_replies(
    runtime: &Arc<CompanyRuntime>,
    company: &CompanyId,
    desk: &str,
    report: &CycleReport,
    // The referral these replies are ANSWERING, when they are answering one.
    //
    // This is the back edge, and it is the host's to carry: "when a host runs
    // a child turn that carried a `ReferralOrigin`, it must pass that origin
    // back in the next `ReferralInput`, or the answer has no way home. Nothing
    // in the library remembers it."
    origin: Option<tinyhivemind_core::referral::ReferralOrigin>,
    // The forward these replies are answering, by its marker's journal
    // sequence. Recorded on the return so the console pairs the two legs by
    // identity rather than by looking for the nearest similar marker — two
    // crossings between the same desks to the same agent are indistinguishable
    // by shape.
    answers: Option<u64>,
    // Depth of the reply being offered — NOT of the child it might spawn.
    //
    // A reply to an operator message is 0, so every operator message starts a
    // fresh chain. Otherwise it is the depth of the turn that produced this
    // reply, which is what makes the count accumulate: the policy compares it
    // against `max_hops` and hands the child `hop + 1`, and that child's own
    // replies come back here at that number. Passing a constant here — as this
    // did — makes every generation claim the same depth, and a bound that never
    // advances bounds nothing.
    hop: u32,
) {
    use tinyhivemind::referral::dispatch_referral;

    let Ok(Some(record)) = runtime.store().load(company).await else {
        return;
    };
    let members = crate::runtime::hivemind::roster_members(&record);
    let people: Vec<tinyhivemind_core::roster::Person> = Vec::new();
    let retired: Vec<String> = Vec::new();
    let roster = tinyhivemind_core::roster::Roster::new(&members, &people, &retired);
    let desks = crate::runtime::hivemind::desk_snapshots(&record);
    let gate = runtime.referral_gate();

    // **The desk's own `[[group_chat]].hive.referral` block, not a constant.**
    //
    // Referral is opt-in per desk and off by default: crossing costs a full
    // model turn on somebody else's desk, and tinyhivemind's own benchmark
    // measured it changing no answer and costing twice the turns on desks that
    // are individually unbiased. A company that says nothing therefore behaves
    // exactly as it did before this existed, which is the direction a mechanism
    // that spends other people's turns should fail in.
    //
    // This replaces a hardcoded `enabled: true` with `max_hops` fixed in the
    // source — a policy no operator could see, let alone change.
    let config = crate::runtime::hivemind::referral_config(&record, desk);
    let policy = config.policy();
    if !policy.enabled {
        return;
    }

    // ONE queue for the whole report, which is what makes `peer_cap` mean
    // anything: the cap counts crossing questions across every reply this turn
    // produced, and a queue rebuilt per reply would start each count at zero.
    let queue = crate::runtime::hivemind::JournalReferralQueue::new(
        runtime.clone(),
        gate.clone(),
        config.peer_cap(),
        policy.max_hops,
        answers,
    );

    for response in &report.responses {
        let (Some(agent), Some(id)) = (response.agent.as_deref(), response.message_id.as_deref())
        else {
            continue;
        };
        let Ok(sequence) = id.parse::<u64>() else {
            continue;
        };
        let mentions = tinyhivemind_core::mention::resolve(
            &response.text,
            None,
            &tinyhivemind_core::mention::MentionAuthor::Agent {
                id: agent.to_string(),
            },
            &roster,
            &desks.set(),
        );
        let input = tinyhivemind_core::referral::ReferralInput {
            key: tinyhivemind_core::dispatch::DispatchKey {
                trigger_sequence: sequence,
            },
            conversation: tinyhivemind_core::dispatch::DispatchConversation {
                desk_id: desk.to_string(),
                thread_root: None,
            },
            author_id: agent.to_string(),
            content: response.text.clone(),
            mentions,
            hop,
            origin: origin.clone(),
        };
        match dispatch_referral(&queue, policy, &input, &roster, &desks.set()).await {
            Ok(outcome) => tracing::info!(
                company = %company,
                desk = %desk,
                author = %agent,
                ?outcome,
                "[referral] decided"
            ),
            Err(err) => tracing::warn!(error = %err, "[referral] decision failed"),
        }
    }
}

#[cfg(test)]
#[path = "operator_readable_responses_test.rs"]
mod readable_responses_test;

/// The same rendering `chat_history` applies, for replies going out on the POST
/// rather than being read back.
///
/// A deliberation turn is journaled with its grammar and cleaned when the
/// history is projected — but a reply returned to the caller never passes
/// through that projection, so the console showed `!support #lazy-load ^3` on
/// a row that arrived live and plain prose on the same row after a reload.
/// Two readers of one message, disagreeing, with a page refresh between them.
///
/// The stored row keeps its markers either way; the fold reads them off the
/// journal, not off this.
fn readable_responses(
    mut responses: Vec<crate::ports::types::OutboundMessage>,
) -> Vec<crate::ports::types::OutboundMessage> {
    for response in &mut responses {
        response.text =
            crate::server::chat_history::readable_moves(std::mem::take(&mut response.text));
    }
    responses
}

/// Awaits a spawned chat turn, turning a task that never finished into an error.
///
/// Mirrors [`crate::company::runtime::join_follow_up`]: a panicked or aborted
/// task is a background-task failure rather than a silent empty reply.
async fn join_chat_turn(
    turn: JoinHandle<Result<(CycleReport, Option<String>), ApiError>>,
) -> Result<(CycleReport, Option<String>), ApiError> {
    match turn.await {
        Ok(result) => result,
        Err(err) => Err(ApiError(OpenCompanyError::BackgroundTask(format!(
            "the chat turn did not finish: {err}"
        )))),
    }
}

/// Journals each reply against the addressed desk.
///
/// Runs inside the spawned turn (issue #882) so the record survives a client or
/// proxy that gave up waiting.
/// The thread an answer belongs in (issue #1890 D part 1).
///
/// `asked_in` is the root the operator's message hung off, and `message_seq` is
/// that message's own position.
///
/// * **Already in a thread** — the answer takes the same root. A follow-up
///   typed inside a thread must not open a thread of its own, or N messages
///   would mean N threads instead of N *topics*.
/// * **Not in one** — the answer takes the message itself as its root, so the
///   exchange becomes a thread rather than two flat lines. This is the change:
///   before it, an answer to an unthreaded question was unparented, and the
///   only threads that existed were ones an operator opened by hand.
///
/// Never `None`, and that is the point: **uniform**. The tempting version
/// decides here — "thread it only if another question arrived while I was
/// working" — which makes `parent` a function of race timing, and `parent` is
/// permanent. Two operators doing the identical thing would get permanently
/// different transcripts on microseconds, and the console renders a reply as it
/// streams, before the backend could know. Whether the pair *reads* as a thread
/// is re-decided on every render instead, by the console's `buildTimeline`.
fn reply_thread(asked_in: Option<EventSeq>, message_seq: EventSeq) -> Option<EventSeq> {
    Some(asked_in.unwrap_or(message_seq))
}

/// Mint a crossing marker for every `desk_dm` a turn sent (#2368).
///
/// # Why post-turn, and why it reads the journal
///
/// A crossing folds onto the row that RAISED it, and for a tool that row is the
/// turn's own reply — composed and journaled after every tool has run. The tool
/// cannot mint its own marker: the sequence it would key on does not exist yet.
///
/// The DM rows, however, are already durable by then — the tool appended them
/// while the turn ran. So this reads them back rather than carrying them out
/// through `TurnOutcome`, `OperatorTurn` and `OutboundMessage`, which is three
/// types widened to move data across one function boundary.
///
/// # Which rows belong to this crossing
///
/// Those in a pair conversation this author is in, below the reply, and above
/// the last marker already minted for that conversation. That last bound is the
/// same rule the fold applies forward — *"the rows between two markers are the
/// rows that marker caused"* — read backwards, so a second `desk_dm` to the
/// same teammate cannot re-claim the first one's rows.
async fn mark_turn_dms(
    runtime: &Arc<CompanyRuntime>,
    id: &CompanyId,
    desk: &str,
    author: &str,
    reply_seq: u64,
) {
    // **A turn taken INSIDE a pair thread is a leg, not a new crossing.**
    //
    // The peer's reply runs as its own turn, journaled to the same `dm:<a>+<b>`
    // key, so it looks exactly like a DM worth marking — and marking it minted
    // a second marker for the one exchange, pointing at a `to_desk` that is the
    // pair thread itself. No operator timeline draws that desk, so the marker
    // rendered nowhere; worse, it became the boundary that bounds the REAL
    // chip, truncating it to the question and cutting off the answer it was
    // minted to show.
    if pair_peer(desk, author).is_some() {
        return;
    }
    // Bounded: a turn's own DMs are within a page of its reply, and a marker
    // that is missed renders no chip rather than a wrong one.
    const SCAN: usize = 256;
    let Ok(recent) = runtime
        .events()
        .read_before(id, Some(EventSeq::new(reply_seq)), SCAN)
        .await
    else {
        return;
    };
    let Some(record) = runtime.store().load(id).await.ok().flatten() else {
        return;
    };
    // Newest first, so the first marker seen for a conversation is the most
    // recent one and everything older than it belongs to an earlier crossing.
    let mut floor: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut rows: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
    for stored in &recent {
        match &stored.event {
            // **This turn's own DMs, and no earlier turn's.**
            //
            // Reading newest-first, the turn under way began at the first
            // `TurnStarted` below its reply; everything older belongs to a turn
            // that already ended. Bounding on the previous MARKER instead let a
            // turn that had failed leak into this one — a live run marked
            // `rows: [39, 46]` where 39 was a question asked by a turn that then
            // errored, so a fresh chip opened with a stale unanswered line.
            CompanyEvent::TurnStarted { .. } => break,
            CompanyEvent::ReferralEnqueued {
                conversation: Some(seen),
                ..
            } => {
                floor.entry(seen.clone()).or_insert(stored.seq.value());
            }
            // **Either party's rows, not only the asker's.**
            //
            // When the peer's turn runs inline the answer lands in this same
            // pair thread during this same turn, authored by the peer. Matching
            // on the asker alone therefore named a range that covered the
            // question and stopped short of the reply, and the chip could only
            // ever say "1 message" — the one shape the range exists to prevent.
            // `pair_peer` still does the gating: it is `None` for anything that
            // is not one of THIS author's pair threads.
            CompanyEvent::AgentReply {
                chat_id, agent_id, ..
            } if pair_peer(chat_id, author).is_some() => {
                let Some(peer) = pair_peer(chat_id, author) else {
                    continue;
                };
                let _ = agent_id;
                if floor
                    .get(chat_id)
                    .is_some_and(|at| stored.seq.value() < *at)
                {
                    continue;
                }
                let at = stored.seq.value();
                rows.entry(chat_id.clone())
                    .and_modify(|(open, _)| *open = (*open).min(at))
                    .or_insert((at, at));
                let _ = peer;
            }
            _ => {}
        }
    }
    for (conversation, (opened, closed)) in rows {
        let Some(peer) = pair_peer(&conversation, author) else {
            continue;
        };
        let event = CompanyEvent::ReferralEnqueued {
            conversation: Some(conversation),
            rows: Some((opened, closed)),
            answers: None,
            from_desk: desk.to_string(),
            from_desk_name: crate::server::chat_history::desk_display_name(&record, desk),
            asker: author.to_string(),
            asker_label: author.to_string(),
            trigger_sequence: reply_seq,
            to_desk: desk.to_string(),
            target: peer,
            returning: false,
        };
        if let Err(error) = runtime.events().append(id, event).await {
            tracing::warn!(
                company = %id,
                error = %error,
                "[speech] a desk_dm could not be marked; it stays durable but renders no chip"
            );
        }
    }
}

/// The other party in a `dm:<a>+<b>` conversation, or `None` when this is not
/// one of `author`'s pair threads.
///
/// Agent ids are snake_case, so `+` separates them unambiguously.
fn pair_peer(conversation: &str, author: &str) -> Option<String> {
    let rest = conversation.strip_prefix("dm:")?;
    let (left, right) = rest.split_once('+')?;
    match (left == author, right == author) {
        (true, false) => Some(right.to_string()),
        (false, true) => Some(left.to_string()),
        _ => None,
    }
}

pub(crate) async fn journal_chat_replies(
    runtime: &Arc<CompanyRuntime>,
    id: &CompanyId,
    desk: &str,
    parent: Option<EventSeq>,
    report: &mut CycleReport,
) {
    // Journal each reply against the addressed desk so desk history can be read
    // back (GraphQL `Chat.history`, WS2c). Single-responder in v1.
    //
    // The append's returned sequence used to be discarded. It is the reply's
    // durable id (issue #364) — the same id `chat/history` gives it on the next
    // reload — so it goes back on the bubble, and a reaction or a thread reply
    // made against a bubble the operator can still see names something every
    // other reader can resolve.
    for response in &mut report.responses {
        // A response that already carries a durable id was journaled by its
        // producer, not by this loop — a hive desk episode journals its own
        // turns and closing report directly (`EpisodeDriver::report`) and
        // hands the report's own sequence back on the bubble precisely so
        // this generic journal-on-return path does not write it a second
        // time under a different sequence. `OutboundMessage::message_id` is
        // documented as "stamped by the chat route after journaling, not
        // produced by a brain" for every other producer, which is exactly
        // what makes its presence here a reliable "already durable" signal
        // rather than something a brain sets for itself.
        if response.message_id.is_some() {
            continue;
        }
        let response_desk = if response.channel == crate::runtime::OPERATOR_CHANNEL {
            desk.to_string()
        } else {
            response.channel.clone()
        };
        let response_parent = (response.channel == crate::runtime::OPERATOR_CHANNEL)
            .then_some(parent)
            .flatten();
        // Scanned host-side from the reply text — the console's picker never
        // touched this message. The author is passed so a teammate naming
        // itself in its own answer does not chip itself.
        let reply_mentions = runtime
            .resolve_mentions(
                &response.text,
                None,
                response
                    .agent
                    .as_deref()
                    .map(|agent| Actor {
                        kind: ActorKind::Agent,
                        id: agent.to_string(),
                    })
                    .as_ref(),
            )
            .await;
        let journaled = runtime
            .events()
            .append(
                id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    // Who this reply names. Rendered as chips and — unlike an
                    // operator message's — never consulted by dispatch, which
                    // is the mention-loop fuse.
                    mentions: reply_mentions.clone(),
                    // Zero, and stays zero while that edge does not exist.
                    mention_depth: 0,
                    // The answer joins the thread its question was asked in,
                    // rather than opening one under the question (issue #364).
                    parent: response_parent,
                    // Issue #246: carry the card this turn opened onto the
                    // durable record, so the console's "card opened" chip
                    // survives a transcript reload instead of living only on
                    // the live POST response. This widens the field's meaning
                    // from "the dispatch that produced this reply" to "the card
                    // this reply is about" — a card-creating reply now shows up
                    // in that card's timeline alongside its dispatch replies,
                    // which is the lineage an operator wants and costs no
                    // schema change.
                    task_id: response.task_id.clone(),
                    chat_id: response_desk.clone(),
                    // Issue #885: the author, falling back to the channel only
                    // when the producer did not name one. `agent_id`'s contract
                    // is "the agent that produced the reply"; `channel` is the
                    // destination, so copying it here journaled every bubble on
                    // the operator channel as though the operator wrote it.
                    agent_id: response
                        .agent
                        .clone()
                        .unwrap_or_else(|| response.channel.clone()),
                    text: response.text.clone(),
                    // Persist the per-bubble timeline so a history reload
                    // rehydrates the tool calls, not just the text.
                    steps: response.steps.clone(),
                    // Persist the structured addresses, never the redacted
                    // display strings in the step timeline.
                    outputs: response.outputs.clone(),
                },
            )
            .await;
        // Best-effort, exactly as it always was: a journal failure must not
        // sink a reply the operator can already read. It only costs the bubble
        // its durable id, which the console reads as "not saved" and refuses to
        // thread or react on — the honest degradation.
        match journaled {
            Ok(seq) => {
                response.message_id = Some(seq.value().to_string());
                // The chip for any `desk_dm` this turn sent. Here rather than in
                // the tool because a crossing folds onto the row that raised it,
                // and that row is the line just journaled above (#2368).
                if let Some(author) = response.agent.as_deref() {
                    mark_turn_dms(runtime, id, &response_desk, author, seq.value()).await;
                }
                // The durable half of a reply's mention, same as an operator
                // message's (issue: mentions). Without this an `@user` an agent
                // types back renders as a chip and nothing else — the badge and
                // the notification both silently missing for whoever it named,
                // which is worst for exactly the person it is meant to reach:
                // offline when the reply lands.
                if !reply_mentions.is_empty() {
                    runtime
                        .notify_mentions(id, &reply_mentions, &seq, None, &response_desk)
                        .await;
                }
            }
            Err(err) => tracing::warn!(
                error = %err,
                "failed to journal a chat reply; the bubble has no durable id"
            ),
        }
    }
}

/// Parses a message id from the wire into the sequence position it names.
///
/// Message ids are stringified sequence positions everywhere this API exposes
/// them, so this is the one place that turns one back — and the one place that
/// refuses. A 400 rather than a silent `None`: a thread reply whose parent was
/// dropped lands in the channel instead, which looks to the operator like the
/// reply went missing.
fn parse_message_id(raw: &str) -> Result<EventSeq, ApiError> {
    raw.trim().parse::<u64>().map(EventSeq::new).map_err(|_| {
        ApiError(OpenCompanyError::InvalidRequest(format!(
            "'{raw}' is not a message id"
        )))
    })
}

/// Resolves who is sending a chat message.
///
/// Chat is the one surface both machines and humans drive, so it accepts
/// either. A signed-in user is attributed to themselves; a platform credential
/// yields `None`, which reads back as "operator" — there is no person behind it
/// to name.
async fn chat_actor(
    headers: &HeaderMap,
    state: &AppState,
    company: &CompanyId,
    peer: Option<std::net::SocketAddr>,
) -> Result<Option<Actor>, crate::server::Rejection> {
    use crate::server::graphql::auth::{GqlAuth, resolve_principal};

    // `peer` is threaded from every one of this function's callers, all the
    // way from their own handler's `MaybePeer` extractor, so `local_owner`'s
    // loopback-peer gate applies on this surface exactly as it does through
    // `CompanyAuth` and the GraphQL handler.
    let auth = resolve_principal(headers, state, Some(company), peer)
        .await
        .map_err(|_| unauthorized_response())?;
    if let Some(resp) = authorize_address(state, &auth, company) {
        return Err(resp.into());
    }
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return Err(resp.into());
    }
    Ok(match auth {
        GqlAuth::User(user) => Some(Actor {
            kind: ActorKind::User,
            id: user.user_id,
        }),
        GqlAuth::Platform(_) => None,
    })
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "unauthorized", "code": "unauthorized" })),
    )
        .into_response()
}

/// `POST /api/v1/companies/{id}/chat`.
async fn operator_chat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(message): Json<ChatMessage>,
) -> Result<ChatOk, crate::server::Rejection> {
    let company = CompanyId::new(&id);
    let by = chat_actor(&headers, &state, &company, peer).await?;
    let runtime = lookup(&state, &id)?;
    chat_and_emit(&state, &company, runtime, message, by)
        .await
        .map_err(|error| IntoResponse::into_response(error).into())
}

/// `POST /api/v1/company/chat` (single-company alias).
async fn operator_chat_single(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(message): Json<ChatMessage>,
) -> Result<ChatOk, crate::server::Rejection> {
    let runtime = sole(&state)?;
    let id = runtime.id().clone();
    let by = chat_actor(&headers, &state, &id, peer).await?;
    chat_and_emit(&state, &id, runtime, message, by)
        .await
        .map_err(|error| IntoResponse::into_response(error).into())
}

/// Query params for `GET .../chat/history`.
#[derive(Debug, Deserialize)]
struct ChatHistoryQuery {
    /// The desk to read, by id or name. Omitted defaults to the operator's
    /// General/"main" line — the console's default thread (issue #65).
    #[serde(default)]
    desk: Option<String>,
    /// Exclusive event cursor. Omitted reads the current tail; passing the
    /// oldest id already held walks backward without rereading newer events.
    #[serde(default)]
    before: Option<u64>,
    /// Maximum transcript messages to return. Capped server-side so a caller
    /// cannot turn one history read back into an unbounded response.
    #[serde(default)]
    limit: Option<usize>,
}

/// One desk-history message, as the console renders it. Mirrors `ChatMessage`
/// in `frontend/src/lib/chat.ts`.
/// Where a crossing referral came from, when another desk caused this message
/// (tinyhivemind P15).
///
/// Mirrors `ReferredFromDto` in `frontend/src/api/types.ts`.
///
/// The labels are **captured with the row** rather than resolved when the
/// transcript is read, for the reason [`SessionAuthor`] captures its own: a
/// desk renamed later must not rewrite what the conversation said at the time.
///
/// [`SessionAuthor`]: tinyhivemind::session::SessionAuthor
/// One line of a crossing, as the console renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReferralLineDto {
    /// Who wrote it, by id.
    author_id: String,
    /// Their display label when the referral was made; empty for this desk's
    /// own agent, whom the console already names.
    author_label: String,
    /// What they said.
    text: String,
    /// True for the question leaving this desk, false for the answer coming
    /// back — which is what lets the console show the two sides differently.
    outbound: bool,
}

/// A crossing folded onto the report that brought it home, so the console can
/// render it as one collapsed line naming both parties and counting the
/// messages.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReferralConversationDto {
    /// The agent on this desk that asked.
    asker_id: String,
    /// Who they asked.
    other_id: String,
    /// And where that person sits — id for the link, name for the label.
    other_desk_id: String,
    other_desk_name: String,
    /// Whether a person was asked rather than a desk — `@name` vs `#desk`.
    direct: bool,
    /// Whether this desk was ASKED rather than doing the asking. Every other
    /// field is named from the asker's side, so without this the label renders
    /// an answering desk's crossing backwards.
    inbound: bool,
    /// The exchange, oldest first. Its length is the count in the label.
    lines: Vec<ReferralLineDto>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AsideLineDto {
    /// The agent that wrote it.
    author_id: String,
    /// What they said, with the `!aside @peer` head already stripped.
    text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AsideConversationDto {
    /// Everyone in it — the author first, then who they addressed.
    members: Vec<String>,
    /// The exchange, oldest first. Its length is the count in the label.
    lines: Vec<AsideLineDto>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReferredFromDto {
    /// The desk that asked, by id — for the link, never for display.
    desk_id: String,
    /// The desk's display name as it stood when the referral was made.
    desk_name: String,
    /// The agent that asked, by id.
    asker_id: String,
    /// That agent's display label as it stood when the referral was made.
    asker_label: String,
    /// The asking message, so the chip links straight to it.
    sequence: u64,
    /// Whether a person was asked rather than a desk, so the chip can name
    /// whoever was actually addressed.
    direct: bool,
    /// Which word the chip uses. `"asked"` on the outbound leg, `"answered"`
    /// when the answer has come home.
    ///
    /// Sent as the word rather than a bool because the console renders it and
    /// nothing else: a `returning: true` would have the render side translating
    /// a host decision back into English, which is how it came to guess in the
    /// first place.
    direction: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatHistoryMessageDto {
    /// The message id (its EventLog sequence position).
    id: String,
    /// The channel the message came in on.
    channel: String,
    /// The author label.
    author: String,
    /// The message text.
    text: String,
    /// **The body as the model wrote it** — [`MessageView::cue_text`], which
    /// is [`Self::text`] before `readable_moves` rewrote the room's grammar
    /// into operator-facing prose.
    ///
    /// `AgentSessionMessageDto` has carried this since the raw-turns view
    /// needed it; this shape did not, so a reader that needs the *moves*
    /// rather than the prose had nothing to read them from on the reload path.
    /// The episode fold is such a reader, which is why a deliberation panel
    /// never survived a refresh.
    ///
    /// Omitted when it is byte-equal to [`Self::text`], which is every row
    /// carrying no move — so the wire shape is unchanged for every reply on
    /// every desk that does not deliberate.
    #[serde(skip_serializing_if = "Option::is_none")]
    cue_text: Option<String>,
    /// Set only when another desk's referral caused this line. Absent on every
    /// ordinary message, so the wire shape is unchanged for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    referred_from: Option<ReferredFromDto>,
    /// The crossing this report brought home, when it brought one. Absent on
    /// every ordinary message, so the wire shape is unchanged for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    referral_conversation: Option<ReferralConversationDto>,
    aside_conversation: Option<AsideConversationDto>,
    /// When it was journaled, epoch millis.
    at_millis: f64,
    /// Whether it is the operator's own message.
    mine: bool,
    /// Whether a **person** wrote this line rather than the runtime (issue
    /// #1734). See [`MessageView::by_person`] for why nothing downstream can
    /// derive it — in particular why `channel == "operator"` cannot, the echo
    /// brain naming its own outbound channel that too.
    ///
    /// Omitted when `false`, which is every agent reply and every message
    /// journaled before the field existed, so the legacy shape is unchanged and
    /// a console reading `undefined` gets today's behaviour.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    by_person: bool,
    /// The scrubbed processing steps behind a company reply, so a rehydrated
    /// transcript renders the same timeline the live turn showed. Omitted when
    /// empty (operator messages, tool-less replies) — keeps the legacy shape.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    steps: Vec<TurnStep>,
    /// The board card this reply is about (issue #246), so a rehydrated
    /// transcript renders the same "card opened" chip the live turn showed.
    /// Omitted when absent — which is every message journaled before the field
    /// existed — so the legacy shape is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// Workspace objects produced by this reply's turn. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    outputs: Vec<crate::ports::types::ChatOutput>,
    /// The message this one replies to (issue #364), so a thread survives a
    /// reload instead of collapsing into the channel. Omitted on a message
    /// posted straight into the channel — which is every message journaled
    /// before threads were persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    /// Who reacted to this message with what (issue #364), one row per person
    /// per emoji. Omitted when nobody has, keeping the legacy shape.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reactions: Vec<ChatReactionDto>,
    /// Who this message names, in reading order. Omitted when it names nobody
    /// — which is every message journaled before mentions existed — so the
    /// legacy shape is unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mentions: Vec<ChatMentionDto>,
    /// Files attached to this message (issue #1682), each a reference into the
    /// company workspace with the store-computed name / mime / size. Omitted
    /// when the message carries none — which is every reply, every system pill,
    /// and every operator message journaled before the field existed — so the
    /// legacy shape is unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<ChatAttachmentDto>,
    /// Whether this row is a classified resolution failure (keys rework
    /// #2306, round-2 review KR-L2-03) — a pinned provider gone or switched
    /// off, a broken company default, a provider with no key, or no model
    /// chosen at all. Absent (reads as falsy) for every ordinary reply and
    /// every other failure class, which keep only `text`'s generic wording,
    /// exactly as before this field existed. See
    /// `docs/key-reworks/in-use-guards.md` §5 for the full field contract.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    user_facing: bool,
    /// One of the codes `docs/key-reworks/in-use-guards.md` §5 names, when
    /// `userFacing` is `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    /// The exact X9 sentence, present only when `userFacing` is `true` —
    /// identical to `text` for a classified failure, carried as its own
    /// field so a reader need not also parse `text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// The agent this failure's pair names, when the classifier could
    /// recover one — see `company::inference::copy::classify`'s own doc for
    /// the one call site (the pin pre-check) that has an id to attach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pair_agent_id: Option<String>,
    /// The provider slug the failure names, when the classifier could
    /// recover one — only `pair_provider_removed` today.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_slug: Option<String>,
}

/// One file attached to a history message (issue #1682). Mirrors `Attachment`
/// in `frontend/src/lib/chat.ts`, and carries only store-authored metadata —
/// the id the payload is reachable at, and the name / mime / size the store
/// computed. The bytes are fetched separately through the hardened
/// `GET …/workspace/blob/{nodeId}` route.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatAttachmentDto {
    /// The workspace node id the payload is stored under — what the console
    /// hands the blob route to download or preview it.
    node_id: String,
    /// The stored file's display name.
    name: String,
    /// The stored payload's media type, so the console decides download-vs-
    /// preview without fetching the bytes.
    mime: String,
    /// The stored payload's exact length in bytes.
    size: u64,
}

impl From<Attachment> for ChatAttachmentDto {
    fn from(attachment: Attachment) -> Self {
        Self {
            node_id: attachment.node_id,
            name: attachment.name,
            mime: attachment.mime,
            size: attachment.size,
        }
    }
}

/// One mention on a history message. Mirrors `Mention` in
/// `frontend/src/lib/chat.ts`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatMentionDto {
    /// The literal span the author typed, so the renderer highlights the text
    /// as written rather than the target's current name.
    text: String,
    /// Byte offset of `text` in the message body.
    offset: usize,
    /// Who was named, as a display label — never a raw user id.
    label: String,
    /// Whether the reading viewer is the one named (or was named by
    /// `@everyone`).
    mine: bool,
    /// Whether this mention renders but pings nobody.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    quiet: bool,
}

impl From<MentionView> for ChatMentionDto {
    fn from(view: MentionView) -> Self {
        Self {
            text: view.text,
            offset: view.offset,
            label: view.label,
            mine: view.mine,
            quiet: view.quiet,
        }
    }
}

/// One person's reaction on a history message. Mirrors `Reaction` in
/// `frontend/src/lib/chat.ts`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatReactionDto {
    /// The emoji.
    emoji: String,
    /// Who reacted, as a display label — never a raw user id.
    by: String,
    /// Whether the reading viewer is the one who reacted.
    mine: bool,
}

impl From<ReactionView> for ChatReactionDto {
    fn from(view: ReactionView) -> Self {
        Self {
            emoji: view.emoji,
            by: view.by_label,
            mine: view.mine,
        }
    }
}

impl From<MessageView> for ChatHistoryMessageDto {
    fn from(view: MessageView) -> Self {
        // Keys rework #2306, round-2 review KR-L2-03: `message` reuses
        // `view.text` rather than a separate stored field — they are
        // identical for a classified failure by construction
        // (`spawn_chat_turn` writes the bare X9 sentence into `text` for
        // exactly this case, and `MessageView::project` classifies that same
        // text back). Cloned before `view.text` moves into the `text` field
        // below.
        let message = view.resolution_user_facing.then(|| view.text.clone());
        // Only when the two differ, which is only on a desk that deliberates:
        // `readable_moves` returns a body carrying no move untouched, so an
        // ordinary reply adds nothing to the wire.
        let cue_text = (view.cue_text != view.text).then(|| view.cue_text.clone());
        Self {
            cue_text,
            aside_conversation: view.aside_conversation.map(|aside| AsideConversationDto {
                members: aside.members,
                lines: aside
                    .lines
                    .into_iter()
                    .map(|line| AsideLineDto {
                        author_id: line.author_id,
                        text: line.text,
                    })
                    .collect(),
            }),
            referral_conversation: view.referral_conversation.map(|crossing| {
                ReferralConversationDto {
                    asker_id: crossing.asker_id,
                    other_id: crossing.other_id,
                    other_desk_id: crossing.other_desk_id,
                    other_desk_name: crossing.other_desk_name,
                    direct: crossing.direct,
                    inbound: crossing.inbound,
                    lines: crossing
                        .lines
                        .into_iter()
                        .map(|line| ReferralLineDto {
                            author_id: line.author_id,
                            author_label: line.author_label,
                            text: line.text,
                            outbound: line.outbound,
                        })
                        .collect(),
                }
            }),
            referred_from: view.referred_from.map(|origin| ReferredFromDto {
                desk_id: origin.desk_id,
                desk_name: origin.desk_name,
                asker_id: origin.asker_id,
                asker_label: origin.asker_label,
                sequence: origin.sequence,
                direct: origin.direct,
                direction: if origin.returning {
                    "answered"
                } else {
                    "asked"
                },
            }),
            id: view.id,
            channel: view.channel,
            author: view.author,
            text: view.text,
            at_millis: view.at_millis,
            mine: view.mine,
            by_person: view.by_person,
            steps: view.steps,
            task_id: view.task_id,
            outputs: view.outputs,
            parent_id: view.parent_id,
            reactions: view
                .reactions
                .into_iter()
                .map(ChatReactionDto::from)
                .collect(),
            mentions: view
                .mentions
                .into_iter()
                .map(ChatMentionDto::from)
                .collect(),
            attachments: view
                .attachments
                .into_iter()
                .map(ChatAttachmentDto::from)
                .collect(),
            user_facing: view.resolution_user_facing,
            message,
            code: view.resolution_code,
            pair_agent_id: view.resolution_pair_agent_id,
            provider_slug: view.resolution_provider_slug,
        }
    }
}

/// Resolves a `?desk=` selector to the `(id, name)` pair `history_for_desk`
/// filters on.
///
/// A selector matching a manifest group chat (by id or name,
/// case-insensitive) resolves to that desk's real id/name pair — same as the
/// GraphQL `chat(id:)` lookup. An unmatched selector (an ad hoc thread id the
/// console addresses with no backing manifest entry, e.g. a static default
/// thread) passes through as both id and name, so history still finds
/// whatever was journaled under that exact string. Omitted resolves to the
/// synthetic General/operator desk.
async fn resolve_desk(
    runtime: &CompanyRuntime,
    desk: Option<&str>,
) -> Result<(String, String), OpenCompanyError> {
    let Some(desk) = desk else {
        return Ok((DEFAULT_DESK.to_string(), DEFAULT_DESK.to_string()));
    };
    let record = runtime.store().load(runtime.id()).await?;
    let matched =
        record.and_then(|record| {
            record.manifest.group_chats.into_iter().find(|chat| {
                chat.id.eq_ignore_ascii_case(desk) || chat.name.eq_ignore_ascii_case(desk)
            })
        });
    Ok(match matched {
        Some(chat) => (chat.id, chat.name),
        None => (desk.to_string(), desk.to_string()),
    })
}

/// Resolves who is reading a desk's history, for the `mine` flag, plus
/// whether they may see an [`MessageView::admin_only`] row (issue #1781
/// review, Codex P1). Reuses [`chat_actor`]'s auth (session cookie or platform
/// credential, tenant address-authorization, temporary-password gate) for the
/// `Viewer` itself, so a history read can never see more than a matching chat
/// send could.
///
/// The admin check is a **second**, independent lookup
/// ([`current_user`](crate::server::users::routes::current_user)) rather than
/// widening [`Actor`] with a role: `Actor` is shared with the *send* path
/// (`OperatorMessage::by`), where a role has no bearing on whether a
/// signed-in human may post, so adding one there would be dead weight on every
/// other caller. Safe to run after `chat_actor` already succeeded — this can
/// only **narrow** what the viewer sees (gate an extra row), never widen
/// their access, so it needs none of `chat_actor`'s own refusal gates
/// (address authorization, temporary-password) repeated: those already ran
/// for this exact request via `chat_actor`, and a `current_user` that somehow
/// disagreed would only make `is_admin` `false`, the fail-safe direction.
async fn history_viewer(
    headers: &HeaderMap,
    state: &AppState,
    company: &CompanyId,
    peer: Option<std::net::SocketAddr>,
) -> Result<(Viewer, bool), crate::server::Rejection> {
    let actor = chat_actor(headers, state, company, peer).await?;
    let is_admin = match &actor {
        // A signed-in human: only an active admin sees an admin-only row.
        Some(actor) if actor.kind == ActorKind::User => {
            crate::server::users::routes::current_user(headers, state, company, peer)
                .await
                .is_some_and(|principal| principal.role.may_administer())
        }
        // No person behind this credential — a platform/machine bearer, or
        // (pre-attribution) nobody at all. `Viewer::Operator` already carries
        // full, unrestricted access everywhere else this type is used; an
        // admin-only row is not a narrower case than the rest of a company's
        // history, which this same credential can already read in full.
        _ => true,
    };
    let viewer = match actor {
        Some(actor) if actor.kind == ActorKind::User => Viewer::User(actor.id),
        _ => Viewer::Operator,
    };
    Ok((viewer, is_admin))
}

/// Shared body for both scope forms of `GET .../chat/history`.
async fn chat_history_response(
    state: &AppState,
    company: &CompanyId,
    runtime: Arc<CompanyRuntime>,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    query: ChatHistoryQuery,
) -> Result<Json<Vec<ChatHistoryMessageDto>>, crate::server::Rejection> {
    let (viewer, is_admin) = history_viewer(headers, state, company, peer).await?;
    let (desk_id, desk_name) = resolve_desk(&runtime, query.desk.as_deref()).await?;
    let limit = query
        .limit
        .unwrap_or(CHAT_HISTORY_PAGE_LIMIT)
        .min(CHAT_HISTORY_PAGE_LIMIT);
    let messages = history_for_desk(
        &runtime,
        &desk_id,
        &desk_name,
        &viewer,
        query.before,
        limit,
        is_admin,
    )
    .await?;
    Ok(Json(
        messages
            .into_iter()
            .map(ChatHistoryMessageDto::from)
            .collect(),
    ))
}

/// `GET /api/v1/companies/{id}/chat/history` — a desk's transcript (issue
/// #65), reusing the same filter + projection as GraphQL `Chat.history` via
/// [`history_for_desk`].
async fn chat_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Query(query): Query<ChatHistoryQuery>,
) -> Result<Json<Vec<ChatHistoryMessageDto>>, crate::server::Rejection> {
    let company = CompanyId::new(&id);
    let runtime = lookup(&state, &id)?;
    chat_history_response(&state, &company, runtime, &headers, peer, query).await
}

/// `GET /api/v1/company/chat/history` (single-company alias).
async fn chat_history_single(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Query(query): Query<ChatHistoryQuery>,
) -> Result<Json<Vec<ChatHistoryMessageDto>>, crate::server::Rejection> {
    let runtime = sole(&state)?;
    let id = runtime.id().clone();
    chat_history_response(&state, &id, runtime, &headers, peer, query).await
}

/// One line of an agent's session, as the console renders it.
///
/// A `ChatHistoryMessageDto` plus where it was said. The reuse is the point:
/// the console's `fromHistory` already maps every field of that type, including
/// the referral and aside collapses, so the session view renders an
/// agent-to-agent exchange with the components that already exist rather than
/// with a second set that would drift from them.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentSessionMessageDto {
    /// The line itself.
    #[serde(flatten)]
    message: ChatHistoryMessageDto,
    /// The channel it was said on, as the rail names it (`#general`, `dm`).
    session_channel: String,
    /// The desk id behind that label, so a row can link to its conversation.
    session_channel_id: String,
    /// **What the agent was told to call this row's author.**
    ///
    /// Not the same string as the flattened `author`, and deliberately so: that
    /// one is the display name a *person* reads and falls back to `"someone"`
    /// where this falls back to the signed-in user's id. The console's raw view
    /// reproduces the cue line the model was handed, and a cue rendered from
    /// the display name would put a name in front of the operator that the
    /// agent never saw. See [`cue_author`](crate::server::chat_history::cue_author).
    cue_author: String,
    /// **The text half of the cue line the model was actually handed** —
    /// before [`readable_moves`](crate::server::chat_history::readable_moves)
    /// rewrote it into operator-facing prose (Codex P2: the raw-turns surface
    /// must show `!support #topic ^3`, not the prose it becomes for a person).
    /// Same reasoning as `cue_author`, for the other half of the line. See
    /// [`MessageView::cue_text`](crate::server::chat_history::MessageView::cue_text).
    cue_text: String,
    /// **The openhuman session these turns belong to** — `{company}:{agent_id}`,
    /// exactly as
    /// [`openhuman_session_key`](crate::session_key::openhuman_session_key)
    /// mints it for the builder that stamps it onto the live session.
    ///
    /// # Why a per-row field and not an envelope
    ///
    /// This is metadata about the *session*, not about the row, so the tidy
    /// shape would be `{ sessionKey, rows: [...] }`. It is a field anyway,
    /// because the route already answers a bare JSON array and every existing
    /// caller — both console surfaces, and anything else reading the documented
    /// route — indexes, filters and maps that array directly. Wrapping it is a
    /// breaking change to a shipped shape in exchange for saving one repeated
    /// string; an added field is one every old caller ignores. The repetition
    /// is bounded and constant: one short string per row of a page already
    /// capped at `CHAT_HISTORY_PAGE_LIMIT`.
    ///
    /// The console renders this and never rebuilds it: a second spelling of a
    /// session's name in TypeScript is a second spelling that can drift from
    /// the one the runtime actually uses.
    openhuman_session_key: String,
}

/// `GET {scope}/agents/{agent_id}/session` — every row on every channel this
/// agent is eligible to read.
///
/// # Why this is one route and not "read the desks yourself"
///
/// The console could fetch `chat/history` per desk and merge. It must not: the
/// set of channels an agent can read is decided by
/// [`agent_channels`](crate::server::chat_history::agent_channels),
/// and that function is also what decides the agent's **own** session. Asking
/// it here is what keeps the page from claiming an agent saw something it did
/// not — one function, two readers, no drift.
///
/// # Eligible is not delivered
///
/// This is channel history the agent **may** read, not a record of what it
/// has **already** been handed. A `desk_dm` journals a row and runs nothing —
/// the recipient reads it on its own next turn, through the per-agent
/// watermark `agent_session::AgentSessionState` tracks. That watermark lives
/// in the live `HarnessPool`, gated behind the `openhuman` feature; this route
/// has no access to it and compiles in every build. So a message queued
/// behind another turn shows up here immediately,
/// same as one the agent answered an hour ago. See
/// `docs/spec/runtime/speech.md#reading-it-back`.
///
/// # The operator sees more than the agent does, deliberately
///
/// Projected with the caller's own [`Viewer`], so an operator reads private
/// asides in full while the agent's session has them narrowed by
/// `Audience::admits`. That asymmetry is the documented rule: privacy here is a
/// deliberation device between agents and never a security boundary.
async fn agent_session_response(
    state: &AppState,
    company: &CompanyId,
    runtime: Arc<CompanyRuntime>,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    agent_id: &str,
    query: ChatHistoryQuery,
) -> Result<Json<Vec<AgentSessionMessageDto>>, crate::server::Rejection> {
    let (viewer, is_admin) = history_viewer(headers, state, company, peer).await?;
    let limit = query
        .limit
        .unwrap_or(CHAT_HISTORY_PAGE_LIMIT)
        .min(CHAT_HISTORY_PAGE_LIMIT);
    let Some(record) = runtime.store().load(runtime.id()).await? else {
        return Ok(Json(Vec::new()));
    };
    let channels = crate::server::chat_history::agent_channels(&record, agent_id);

    // One page per channel, then merged by sequence. Each page is already
    // bounded by `limit`, so the merge is bounded by `channels × limit` before
    // the tail cut below — and an agent sits on a handful of desks, not a
    // hundred.
    // Minted once, by the one function that names a session, and copied onto
    // every row. See `AgentSessionMessageDto::openhuman_session_key`.
    let session_key = crate::session_key::openhuman_session_key(company, agent_id);
    let mut rows: Vec<AgentSessionMessageDto> = Vec::new();
    for channel in &channels {
        let messages = history_for_desk(
            &runtime,
            &channel.id,
            &channel.name,
            &viewer,
            query.before,
            limit,
            is_admin,
        )
        .await?;
        for message in messages {
            // Read before the conversion: `ChatHistoryMessageDto::from` takes
            // the view by value, and neither field below is one it carries —
            // they are what the agent was handed, not what the reader is.
            let cue_author = message.cue_author.clone();
            let cue_text = message.cue_text.clone();
            rows.push(AgentSessionMessageDto {
                message: ChatHistoryMessageDto::from(message),
                cue_author,
                cue_text,
                session_channel: channel.label.clone(),
                session_channel_id: channel.id.clone(),
                openhuman_session_key: session_key.clone(),
            });
        }
    }
    // Journal order, oldest first — the order the agent itself experienced.
    // `id` is the sequence the row was journaled under, so it sorts numerically
    // rather than lexically; a string sort would put [10] before [9].
    rows.sort_by_key(|row| row.message.id.parse::<u64>().unwrap_or(0));
    rows.dedup_by(|a, b| a.message.id == b.message.id);
    if rows.len() > limit {
        rows.drain(..rows.len() - limit);
    }
    Ok(Json(rows))
}

/// `GET /api/v1/companies/{id}/agents/{agent_id}/session`.
async fn agent_session(
    State(state): State<AppState>,
    Path((id, agent_id)): Path<(String, String)>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Query(query): Query<ChatHistoryQuery>,
) -> Result<Json<Vec<AgentSessionMessageDto>>, crate::server::Rejection> {
    let company = CompanyId::new(&id);
    let runtime = lookup(&state, &id)?;
    agent_session_response(&state, &company, runtime, &headers, peer, &agent_id, query).await
}

/// `GET /api/v1/company/agents/{agent_id}/session` (single-company alias).
async fn agent_session_single(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Query(query): Query<ChatHistoryQuery>,
) -> Result<Json<Vec<AgentSessionMessageDto>>, crate::server::Rejection> {
    let runtime = sole(&state)?;
    let id = runtime.id().clone();
    agent_session_response(&state, &id, runtime, &headers, peer, &agent_id, query).await
}

/// The wire shape of `GET {scope}/chat/attribution-audit` (issue #885).
#[derive(Debug, serde::Serialize)]
struct AttributionAuditDto {
    /// Every `AgentReply` in the journal.
    replies: usize,
    /// Those whose stored author names no roster teammate.
    affected: usize,
    /// The distinct bad values with a count each, so an operator can see whether
    /// they are all `operator` (the #885 shape) or whether something else is
    /// also writing a non-agent into the field.
    by_agent_id: std::collections::BTreeMap<String, usize>,
}

/// `GET {scope}/chat/attribution-audit` — the blast radius of issue #885.
///
/// Exists because "we do not know how many rows are wrong" is not an acceptable
/// end state for a data-integrity bug, and the answer needs a journal to count
/// against — which no test fixture and no source checkout has.
///
/// **Counts, never repairs.** The overwritten author is not recoverable from
/// anything on disk; see [`channel_attributed_replies`] for the full argument.
///
/// Gated by the same reader check the sibling transcript route uses, and returns
/// strictly less: counts and agent-id strings, never message text.
async fn attribution_audit_response(
    state: &AppState,
    company: &CompanyId,
    runtime: Arc<CompanyRuntime>,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> Result<Json<AttributionAuditDto>, crate::server::Rejection> {
    let (_viewer, is_admin) = history_viewer(headers, state, company, peer).await?;
    let record = runtime
        .store()
        .load(runtime.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.to_string()))?;
    let audit = channel_attributed_replies(&runtime, &record, is_admin).await?;
    Ok(Json(AttributionAuditDto {
        replies: audit.replies,
        affected: audit.affected,
        by_agent_id: audit.by_agent_id,
    }))
}

async fn attribution_audit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
) -> Result<Json<AttributionAuditDto>, crate::server::Rejection> {
    let company = CompanyId::new(&id);
    let runtime = lookup(&state, &id)?;
    attribution_audit_response(&state, &company, runtime, &headers, peer).await
}

/// `GET /api/v1/company/chat/attribution-audit` (single-company alias).
async fn attribution_audit_single(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
) -> Result<Json<AttributionAuditDto>, crate::server::Rejection> {
    let runtime = sole(&state)?;
    let id = runtime.id().clone();
    attribution_audit_response(&state, &id, runtime, &headers, peer).await
}

/// Body for `POST {scope}/chat/messages/{seq}/reactions` (issue #364).
#[derive(Debug, Deserialize)]
struct ReactionBody {
    /// The emoji to set or clear.
    emoji: String,
    /// `true` to set the reaction, `false` to clear it. Explicit rather than a
    /// toggle so the request is idempotent: a retry, a double tap, or two
    /// consoles racing all converge on what the caller asked for.
    on: bool,
}

/// The longest emoji this route accepts, in bytes.
///
/// A ZWJ sequence (a flag, a family, a profession with a skin tone) is a
/// handful of code points, so the cap has to be well above one character — but
/// this field ends up in an append-only journal read by the operator
/// projection, and nothing about "a reaction" needs more room than a grapheme
/// cluster or two.
const REACTION_MAX_BYTES: usize = 64;

/// Checks an emoji is something a person could have tapped.
///
/// Deliberately **not** a Unicode emoji-property check: the console's palette is
/// its own, a future one may offer more, and refusing an emoji this host has
/// never heard of would break that for no safety gain. What it does refuse is
/// what would make a reaction a smuggling channel — an empty string, a blob, and
/// any control character (a newline in particular, which would let one journal
/// line pretend to be two).
fn validate_emoji(emoji: &str) -> Result<(), ApiError> {
    let invalid = |why: &str| {
        Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "a reaction {why}"
        ))))
    };
    if emoji.trim().is_empty() {
        return invalid("needs an emoji");
    }
    if emoji.len() > REACTION_MAX_BYTES {
        return invalid("must be a single emoji, not a message");
    }
    if emoji.chars().any(char::is_control) {
        return invalid("cannot contain control characters");
    }
    Ok(())
}

/// Shared body for both scope forms of the reaction route.
///
/// Authorized through [`chat_actor`] — the same gate a *send* passes. Reacting
/// is writing into a company's transcript, so it can be neither easier nor
/// harder than saying something in it.
async fn react_to_message(
    state: &AppState,
    company: &CompanyId,
    runtime: Arc<CompanyRuntime>,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    seq: String,
    body: ReactionBody,
) -> Result<StatusCode, crate::server::Rejection> {
    let by = chat_actor(headers, state, company, peer).await?;
    let message_seq = parse_message_id(&seq)?;
    validate_emoji(&body.emoji)?;
    // The target must be a message. Without this the route would happily hang a
    // reaction off an approval, a lifecycle change, or a sequence position that
    // has never existed — none of which any reader could render, and all of
    // which would sit in the log forever claiming otherwise.
    let target = runtime.events().read_from(company, message_seq, 1).await?;
    let matched = target.first().filter(|stored| stored.seq == message_seq);
    let is_message = matched.is_some_and(|stored| {
        matches!(
            stored.event,
            CompanyEvent::OperatorMessage { .. } | CompanyEvent::AgentReply { .. }
        )
    });
    if !is_message {
        return Err(
            ApiError(OpenCompanyError::NotFound(format!("no chat message {seq}")))
                .into_response()
                .into(),
        );
    }
    // An owner-fallback report is admin-only exactly as it is on reload
    // (`history_for_desk`) and over the live SSE feed (`project_event_for_viewer`,
    // issue #1781 review, Codex P1) — a Member must not be able to react to a
    // message they cannot read. Refused with the same 404 the missing-target
    // branch above answers, not a 403: distinguishing "hidden" from "does not
    // exist" would let a Member enumerate which sequence numbers hold an
    // admin-only report by probing this endpoint, which is exactly the gap the
    // sequence-id-based `seq` param opens (PR #1781 review).
    let admin_only = matches!(
        matched.map(|stored| &stored.event),
        Some(CompanyEvent::AgentReply { agent_id, .. })
            if agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR
    );
    if admin_only {
        let is_admin = match &by {
            Some(actor) if actor.kind == ActorKind::User => {
                crate::server::users::routes::current_user(headers, state, company, peer)
                    .await
                    .is_some_and(|principal| principal.role.may_administer())
            }
            // No person behind this credential — a platform/machine bearer —
            // already carries full, unrestricted access everywhere else this
            // distinction is drawn (`history_viewer`, `ScopedCompany::is_admin`).
            _ => true,
        };
        if !is_admin {
            return Err(
                ApiError(OpenCompanyError::NotFound(format!("no chat message {seq}")))
                    .into_response()
                    .into(),
            );
        }
    }
    runtime
        .events()
        .append(
            company,
            CompanyEvent::ReactionToggled {
                message_seq,
                emoji: body.emoji,
                on: body.on,
                by,
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/companies/{id}/chat/messages/{seq}/reactions` — set or clear
/// one reaction on one message (issue #364).
async fn react_to_message_scoped(
    State(state): State<AppState>,
    Path((id, seq)): Path<(String, String)>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(body): Json<ReactionBody>,
) -> Result<StatusCode, crate::server::Rejection> {
    let company = CompanyId::new(&id);
    let runtime = lookup(&state, &id)?;
    react_to_message(&state, &company, runtime, &headers, peer, seq, body).await
}

/// `POST /api/v1/company/chat/messages/{seq}/reactions` (single-company alias).
async fn react_to_message_single(
    State(state): State<AppState>,
    Path(seq): Path<String>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(body): Json<ReactionBody>,
) -> Result<StatusCode, crate::server::Rejection> {
    let runtime = sole(&state)?;
    let id = runtime.id().clone();
    react_to_message(&state, &id, runtime, &headers, peer, seq, body).await
}

/// The operator's thread-scoped review verdict on a settled `in_review`
/// dispatch card. Mirrors `ChatReviewRequest` in `frontend/src/api/types.ts`.
///
/// This is **not** the native-tool approval gate (`resolveApproval`): that
/// settles a parked tool call, while this settles the board card the origin
/// thread is reviewing.
#[cfg(feature = "openhuman")]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatReviewRequest {
    /// The origin conversation — the desk/channel id — whose in-review
    /// dispatch card this verdict settles.
    chat_id: String,
    /// The clicked pill's card id. A desk can have more than one card
    /// `in_review` at once, so the verdict is bound to this specific card
    /// rather than resolved by picking the desk's most-recently-updated one.
    task_id: String,
    /// `approve` finishes the card; `revise` re-runs it with `note`, on the
    /// same path a chat reply of feedback takes.
    decision: String,
    /// The reviewer's note: recorded on the card, and the instruction the
    /// re-run reads back on a `revise`.
    #[serde(default)]
    note: Option<String>,
}

/// The card a review verdict left behind, so the console can reconcile its
/// optimistic move. Mirrors `ChatReviewReceipt` in `frontend/src/api/types.ts`.
#[cfg(feature = "openhuman")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatReviewReceipt {
    /// The reviewed card's id.
    task_id: String,
    /// The column it landed in: `done` on approve, `in_progress` on revise —
    /// or `in_review`, unchanged, on a revise with a blank note.
    column: String,
}

/// `POST {scope}/chat/review` — settle the thread's in-review dispatch card
/// per the operator's verdict.
#[cfg(feature = "openhuman")]
async fn review_card(
    scope: ScopedCompany,
    Json(body): Json<ChatReviewRequest>,
) -> Result<Json<ChatReviewReceipt>, crate::server::Rejection> {
    let decision = crate::harness::built_in::lifecycle::ReviewDecision::parse(&body.decision)
        .ok_or_else(|| {
            ApiError(crate::error::OpenCompanyError::InvalidRequest(format!(
                "unknown review decision '{}'",
                body.decision
            )))
        })?;
    let _serialized = scope.runtime.task_writes.lock().await;
    let card = scope
        .runtime
        .review_card_in_review(&body.task_id, &body.chat_id)
        .await
        .map_err(ApiError)?
        .ok_or_else(|| {
            ApiError(crate::error::OpenCompanyError::NotFound(
                "no card is awaiting review in this conversation".to_string(),
            ))
        })?;
    let updated = scope
        .runtime
        .apply_review_decision(&card, decision, body.note.as_deref(), scope.actor.as_ref())
        .await
        .map_err(ApiError)?;
    Ok(Json(ChatReviewReceipt {
        task_id: updated.id,
        column: updated.column,
    }))
}

/// `GET /api/v1/companies/{id}/approvals`.
async fn list_approvals(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ApprovalSummary>>, crate::server::Rejection> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp.into());
    }
    let runtime = lookup(&state, &id)?;
    // Membership got you the list; role decides whether you may read what is in
    // it (issue #618). Ownership is resolved before either (#1891): the queue
    // is joined to cards by its consumers, and since the board card decides in
    // place, handing out the raw park stamp would let an operator resolve
    // another card's request from this one.
    Ok(Json(crate::server::approval_visibility::for_principal(
        &auth,
        runtime.pending_approvals_resolved().await,
    )))
}

/// `GET /api/v1/company/approvals` (single-company alias).
async fn list_approvals_single(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<ApprovalSummary>>, crate::server::Rejection> {
    let runtime = sole(&state)?;
    // The sole company IS the addressed one, so the principal is checked
    // against it exactly as on the `{id}` form.
    if let Some(resp) = authorize_address(&state, &auth, runtime.id()) {
        return Err(resp.into());
    }
    // Same contents rule as the `{id}` form (issue #618) — and the same
    // ownership resolution (#1891). The two handlers are the same read behind
    // two addressing forms, and either applied to only one of them would be a
    // hole rather than a boundary.
    Ok(Json(crate::server::approval_visibility::for_principal(
        &auth,
        runtime.pending_approvals_resolved().await,
    )))
}

/// The operator's resolution of a parked approval.
///
/// `verdict` stays `approve`/`deny`; the api.md wire enum gains no `edit`
/// verdict. Instead, an optional `amended_payload` paired with an `approve`
/// verdict routes to the approve-with-edit path. Pairing `amended_payload` with
/// `deny` is a contradiction and is rejected as a 400.
#[derive(Debug, Deserialize)]
struct ResolveApproval {
    /// `approve` or `deny`.
    verdict: Verdict,
    /// An optional payload edit; overlaid onto the parked effect on `approve`.
    #[serde(default)]
    amended_payload: Option<serde_json::Value>,
    /// Answer as soon as the verdict is durable, rather than holding the
    /// response open for the agent's follow-up turn (issue #383).
    ///
    /// Defaults to `false`, which keeps the response byte-identical to what
    /// every existing caller receives — a [`ChatResponse`] carrying the
    /// follow-up cycle's messages. Setting it swaps the body for a
    /// [`ResolveReceiptDto`] and lets the continuation arrive on the event
    /// stream's `agent_reply` frame instead, which is where a console that is
    /// already subscribed would rather read it anyway.
    ///
    /// Either way the resolve now survives a dropped connection — the
    /// drop-safety comes from `CompanyRuntime::resolve_approval_spawned`, not
    /// from this flag. What the flag buys is not having to *wait*: a turn slower
    /// than the proxy's read timeout no longer produces a gateway error page
    /// over a decision that was recorded seconds earlier (issue #380).
    #[serde(default)]
    detach: bool,
    /// What this approval buys (issue #374). Absent is
    /// [`ResolveScope::Once`] — today's behaviour, so every existing caller is
    /// unaffected without changing a byte of its body.
    #[serde(default)]
    scope: Option<ResolveScope>,
    /// How long a `tool`-scoped grant lasts, in milliseconds from now.
    ///
    /// **Mandatory with `scope: "tool"`, and capped at seven days.** A request
    /// past the cap is a 400, never a silent clamp: quietly shortening a
    /// duration the operator chose would leave them believing a permission is
    /// live when it lapsed days earlier.
    #[serde(default)]
    expires_in_millis: Option<u64>,
    /// Which of the four things the operator asked a parked **blocker** to do:
    /// `retry`, `amend`, `skip` or `cancel`.
    ///
    /// It **narrows** the mandatory two-value `verdict` rather than replacing
    /// it, the same shape [`amended_payload`](Self::amended_payload) uses: the
    /// approve/deny it must be paired with is the one
    /// [`BlockerVerdict::event_verdict`](crate::ports::blockers::BlockerVerdict::event_verdict)
    /// lowers it onto, and a pair that disagrees is a 400. Absent leaves the
    /// resolve exactly as it was.
    ///
    /// A `String` rather than the enum so an unrecognised token is an explicit
    /// 400 naming the four it could have been, instead of a serde failure.
    #[serde(default)]
    blocker_verdict: Option<String>,
    /// The words an `amend` re-enters the stopped step carrying.
    ///
    /// Mandatory and non-blank with `blocker_verdict: "amend"`, refused with
    /// any other verdict. A blank amend is a 400 rather than a downgrade to a
    /// retry: the step stopped for want of these words, so re-running it
    /// without them repeats the failure the operator thought they had answered.
    #[serde(default)]
    blocker_answer: Option<String>,
}

/// The wire form of [`GrantScope`].
///
/// A closed enum rather than a free string, so an unrecognised scope is a
/// deserialization failure at the edge instead of something that silently
/// degrades to `once` — an operator who asked for a standing permission and
/// quietly got a single call would not find out until the next card appeared.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ResolveScope {
    /// One call, argument-exact. The default.
    Once,
    /// This tool, for this teammate, until `expires_in_millis` from now.
    Tool,
}

/// Validates the requested scope and turns it into a [`GrantScope`] with an
/// absolute deadline (issue #374).
///
/// Every refusal here happens **before** the runtime is touched, so a bad
/// request leaves the approval parked and journals no verdict. The contradictions
/// are refused rather than resolved in the caller's favour:
///
/// * **with `amended_payload`** — an argument edit is by definition an
///   exact-call approval ("this, but with my correction"), and a standing grant
///   admits any arguments. The two say opposite things about the same request.
/// * **with no duration, or zero** — the expiry is mandatory. A grant with no
///   deadline is the silent accumulation this issue exists to prevent.
/// * **past the cap** — 400, never a clamp. See
///   [`MAX_STANDING_GRANT_MILLIS`].
///
/// A duration on `once` is refused too: it would otherwise be dropped on the
/// floor, leaving the operator believing they had bought a week.
fn grant_scope(body: &ResolveApproval) -> Result<GrantScope, ApiError> {
    let bad = |msg: &str| ApiError(OpenCompanyError::InvalidRequest(msg.to_string()));
    match body.scope.unwrap_or(ResolveScope::Once) {
        ResolveScope::Once => {
            if body.expires_in_millis.is_some() {
                return Err(bad(
                    "expires_in_millis only applies to scope \"tool\"; a single-use approval \
                     covers one call and does not last",
                ));
            }
            Ok(GrantScope::Once)
        }
        ResolveScope::Tool => {
            if body.verdict == Verdict::Approve && body.amended_payload.is_some() {
                return Err(bad(
                    "amended_payload cannot accompany scope \"tool\": editing the arguments \
                     approves one exact call, while a standing grant admits any arguments",
                ));
            }
            let Some(duration) = body.expires_in_millis.filter(|d| *d > 0) else {
                return Err(bad(
                    "scope \"tool\" requires a positive expires_in_millis; a standing \
                     permission must have a deadline",
                ));
            };
            if duration > MAX_STANDING_GRANT_MILLIS {
                return Err(bad(&format!(
                    "expires_in_millis must be at most {MAX_STANDING_GRANT_MILLIS} \
                     (seven days); asked for {duration}"
                )));
            }
            Ok(GrantScope::Tool {
                // Absolute, resolved once here. A duration re-based on every
                // read would drift, and a deadline is what the operator was
                // shown.
                expires_at_millis: crate::ports::now_millis().saturating_add(duration),
            })
        }
    }
}

/// Validates the four-way blocker verdict a resolve carries, if any.
///
/// Every refusal here happens **before** the runtime is touched, so a bad
/// request leaves the blocker parked and journals no verdict. What is refused:
///
/// * an unrecognised token — named, rather than a serde failure;
/// * a `verdict`/`blocker_verdict` pair that disagree, judged by
///   [`BlockerVerdict::event_verdict`];
/// * `amend` with a blank or absent answer, and an answer sent with any other
///   verdict;
/// * `blocker_answer` with no `blocker_verdict` at all;
/// * pairing with `amended_payload` — one edits a gated call's arguments, the
///   other answers a question, and no approval is both;
/// * pairing with `scope: "tool"` — a blocker is a question, and answering one
///   grants no standing permission.
///
/// A build without the `openhuman` feature has no blocker resume to reach, so
/// it refuses the field outright rather than accepting and ignoring it.
fn blocker_verdict(body: &ResolveApproval) -> Result<Option<BlockerVerdict>, ApiError> {
    let bad = |msg: String| ApiError(OpenCompanyError::InvalidRequest(msg));
    let answer = body.blocker_answer.as_deref();
    let Some(word) = body.blocker_verdict.as_deref() else {
        if answer.is_some() {
            return Err(bad(
                "blocker_answer needs a blocker_verdict: words with no verdict do not say what \
                 the stopped step should do"
                    .to_string(),
            ));
        }
        return Ok(None);
    };
    #[cfg(not(feature = "openhuman"))]
    return Err(bad(format!(
        "blocker_verdict {word:?} is not supported by this build: it has no blocker resume to \
         answer"
    )));
    #[cfg(feature = "openhuman")]
    {
        let Some(verdict) = BlockerVerdict::from_wire(word) else {
            return Err(bad(format!(
                "unknown blocker_verdict {word:?}; expected \"retry\", \"amend\", \"skip\" or \
                 \"cancel\""
            )));
        };
        let owed = verdict.event_verdict();
        if owed != body.verdict {
            return Err(bad(format!(
                "blocker_verdict {:?} is a {:?}, so it cannot accompany verdict {:?}",
                verdict.as_str(),
                owed,
                body.verdict
            )));
        }
        if verdict == BlockerVerdict::Amend {
            if !answer.is_some_and(|words| !words.trim().is_empty()) {
                return Err(bad(
                    "blocker_verdict \"amend\" needs a non-empty blocker_answer: the step \
                     stopped for want of an answer, so re-entering it without one repeats the \
                     failure"
                        .to_string(),
                ));
            }
        } else if answer.is_some() {
            return Err(bad(format!(
                "blocker_answer only accompanies blocker_verdict \"amend\"; {:?} carries no \
                 words back into the step",
                verdict.as_str()
            )));
        }
        if body.amended_payload.is_some() {
            return Err(bad(
                "blocker_verdict cannot accompany amended_payload: one answers a question, the \
                 other edits a gated call's arguments"
                    .to_string(),
            ));
        }
        if body.scope == Some(ResolveScope::Tool) {
            return Err(bad(
                "blocker_verdict cannot accompany scope \"tool\": answering a blocker grants no \
                 standing permission"
                    .to_string(),
            ));
        }
        Ok(Some(verdict))
    }
}

/// One standing permission, as the console lists it (issue #374).
///
/// Carries **no arguments**, because a standing grant has none — so this route
/// opens no second redaction surface and #372's payload redactor keeps its
/// single call site.
#[derive(Debug, Serialize)]
struct StandingGrantDto {
    /// The grant id — what `DELETE …/grants/{gid}` addresses.
    id: String,
    /// The teammate it was granted to.
    agent: String,
    /// The tool it admits.
    tool: String,
    verdict: Verdict,
    /// Who granted it: a signed-in user, or the platform credential.
    granted_by: Actor,
    /// Epoch-millis it was granted.
    at_millis: u64,
    /// Epoch-millis it stops admitting calls.
    expires_at_millis: u64,
    /// The slice of the tool it is confined to, when the tool's name is not the
    /// whole of what it can do (issue #457) — a Composio toolkit, or absent.
    ///
    /// On the wire because a permission an operator cannot read is a permission
    /// they cannot decide to revoke: a row saying only "act in one of its
    /// connected accounts" does not tell them the grant reaches GitHub and not
    /// their mailbox. Absent — not `null` — when there is nothing to narrow, so
    /// the pre-#457 shape is byte-identical for every other tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    /// The authored workflow allowed to redeem it (issue #1098), when the grant
    /// is to a workflow rather than a teammate.
    ///
    /// On the wire for the same reason `scope` is: `agent` is empty on a
    /// workflow permission, so without this the console would read the row as a
    /// nameless teammate and could not tell two workflows holding the same
    /// tool/scope apart. Absent — not `null` — on every teammate grant, so the
    /// pre-#1098 wire shape is byte-identical for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<String>,
}

impl From<crate::runtime::grants::StandingGrant> for StandingGrantDto {
    fn from(g: crate::runtime::grants::StandingGrant) -> Self {
        Self {
            id: g.id.to_string(),
            agent: g.agent,
            tool: g.tool,
            verdict: g.verdict,
            granted_by: g.granted_by,
            at_millis: g.at_millis,
            expires_at_millis: g.expires_at_millis,
            scope: g.scope,
            workflow: g.workflow,
        }
    }
}

/// `GET {scope}/grants` — the live standing permissions, newest first.
async fn list_grants(scope: ScopedCompany) -> Json<Vec<StandingGrantDto>> {
    Json(
        scope
            .runtime
            .standing_grants()
            .into_iter()
            .map(StandingGrantDto::from)
            .collect(),
    )
}

/// `DELETE {scope}/grants/{gid}` — take a standing permission back.
///
/// Takes effect on the **next** policy check; a call already admitted is not
/// aborted. 404 when there is nothing to revoke — already revoked, or expired —
/// rather than reporting success over a no-op.
///
/// Admin, matching `DELETE {scope}/tools/grants` on the neighbouring plane
/// (issue #2169). Both objects are a permission an operator granted, and a
/// grant one person made should not be undone by anyone who happens to be in
/// the company: a standing permission is often the thing keeping an unattended
/// desk working, so revoking it is a change to how the company runs rather than
/// a tidy-up. Revoking fails in the safe direction, which is why this was easy
/// to leave at member level and worth correcting anyway.
///
/// `GET {scope}/grants` stays readable by any member, deliberately, for the
/// same consistency: `GET {scope}/tools/grants` is member-readable and
/// discloses the same shape of fact.
async fn revoke_grant(
    scope: AdminScopedCompany,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<StatusCode, ApiError> {
    let gid = params
        .get("gid")
        .cloned()
        .ok_or_else(|| ApiError(OpenCompanyError::InvalidRequest("missing grant id".into())))?;
    // Always identified — `AdminScopedCompany::actor` covers the machine
    // principal as well, so this write is never anonymous.
    let by = scope.actor();
    let revoked = scope
        .runtime
        .revoke_standing_grant(&GrantId::new(gid.clone()), by)
        .await?;
    if revoked {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(OpenCompanyError::NotFound(format!(
            "standing permission {gid}"
        ))))
    }
}

/// The actor for a request carrying a machine credential rather than a person.
fn platform_actor() -> Actor {
    Actor {
        kind: ActorKind::Operator,
        id: "platform".to_string(),
    }
}

/// The answer to a detached resolve: the verdict is durable, that is all this
/// claims. The agent's continuation arrives afterwards on the event stream's
/// `agent_reply` frame, which the console already projects and consumes.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolveReceiptDto {
    /// Always `true` — a non-`true` receipt is an error response instead.
    /// Present so the body is self-describing rather than an empty object.
    recorded: bool,
    /// Whether there was nothing left to resolve, because a previous request (or
    /// a double-click) already did. Not a failure: issue #243 made the second
    /// resolve a no-op that mints no second grant, and saying so lets the console
    /// render it as the success it is.
    already_resolved: bool,
    /// How many OTHER decisions the turn behind this approval is still blocked
    /// on (issue #561).
    ///
    /// Since #469 a turn continues once, when the last decision it parked
    /// lands. So on a turn that parked four calls, three of the operator's four
    /// clicks release nothing — and the console said "the agent is completing
    /// the action" for all four. This is what lets it say the true thing
    /// instead. `0` means this decision released the turn.
    still_awaiting: usize,
    /// **Which** of the end states this resolve actually reached (issue #1449):
    /// `"settled"`, `"already_resolved"`, or `"expired"`.
    ///
    /// `already_resolved` above is kept and still means what it always did —
    /// there was nothing left to resolve — so a console predating this field
    /// behaves byte for byte as it did. What it could never express is
    /// `expired`: the approval **was** still parked, and the host default-denied
    /// it because its deadline had passed. Before this the receipt had no shape
    /// for that at all, so the console rendered the one thing it could — the
    /// success line — over a decision the host had refused.
    ///
    /// A string rather than a second boolean because the states are mutually
    /// exclusive: two booleans can spell combinations that cannot happen, and
    /// every reader would have to know which ones are real.
    outcome: &'static str,
    /// Every approval this one resolve settled, when it settled more than the
    /// one addressed.
    ///
    /// A blocker answered here fans its verdict to its whole root-cause group,
    /// exactly as answering it in a DM does, so the console has to drop the
    /// siblings too rather than leave cards for questions the host has already
    /// retired. Skipped when empty, which is every non-blocker resolve.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    settled_ids: Vec<String>,
}

/// Answers a parked blocker with the operator's four-way verdict, fanning it to
/// the blocker's whole root-cause group and naming what it settled.
///
/// Reaches the same bank-arm-settle primitive a DM answer reaches, so a blocker
/// answered on two surfaces cannot settle differently. A `blocker_verdict` on an
/// approval that is not a parked blocker is a 400 here, not a resolve: it would
/// otherwise silently fall back to the two-value path and lose the operator's
/// verdict.
#[cfg(feature = "openhuman")]
async fn resolve_blocker(
    runtime: &Arc<CompanyRuntime>,
    id: &ApprovalId,
    verdict: BlockerVerdict,
    answer: &str,
    actor: Actor,
    settled_ids: &mut Vec<String>,
) -> Result<(ResolveReceipt, JoinHandle<crate::Result<CycleReport>>), ApiError> {
    let Some(group) = runtime.parked_blocker_group(id) else {
        // Already settled — by another tab, a double-click, or this very
        // request racing a sibling's group fan-out. Ordinary approvals return
        // 200 in this race (issue #243); a blocker must too, or a decision
        // that landed successfully reports as a failure.
        if let Some(answer) = runtime.already_resolved_blocker_receipt(id) {
            *settled_ids = Vec::new();
            return Ok(answer);
        }
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "approval {id} is not a parked blocker, so it has no blocker_verdict to answer"
        ))));
    };
    *settled_ids = group.iter().map(ToString::to_string).collect();
    Ok(runtime
        .apply_blocker_reply_spawned(&group, id, verdict, answer, Some(&actor))
        .await?)
}

/// The refusal a build with no blocker resume owes: `blocker_verdict` has
/// already been rejected by [`blocker_verdict`], so nothing reaches here.
#[cfg(not(feature = "openhuman"))]
async fn resolve_blocker(
    _runtime: &Arc<CompanyRuntime>,
    _id: &ApprovalId,
    _verdict: BlockerVerdict,
    _answer: &str,
    _actor: Actor,
    _settled_ids: &mut Vec<String>,
) -> Result<(ResolveReceipt, JoinHandle<crate::Result<CycleReport>>), ApiError> {
    Err(ApiError(OpenCompanyError::InvalidRequest(
        "blocker_verdict is not supported by this build".to_string(),
    )))
}

async fn run_resolve(
    state: &AppState,
    company: &CompanyId,
    runtime: Arc<CompanyRuntime>,
    approval_id: String,
    body: ResolveApproval,
    actor: Actor,
) -> Result<Response, ApiError> {
    runtime.ensure_running().await?;
    // Issue #374: validated before the runtime is touched, so a refused scope
    // leaves the approval parked with no verdict journaled. The blocker verdict
    // is validated on the same terms and for the same reason.
    let blocker = blocker_verdict(&body)?;
    let scope = grant_scope(&body)?;
    let id = ApprovalId::new(approval_id);
    // Every approval this resolve settled, when it settled more than the one
    // addressed — a blocker fans to its root-cause group.
    let mut settled_ids: Vec<String> = Vec::new();
    // The verdict is settled inline; only the follow-up cycle is on the handle.
    // So by the time this returns — in either mode — the decision is journaled
    // and any grant is minted.
    let (receipt, follow_up) = match blocker {
        Some(verdict) => {
            resolve_blocker(
                &runtime,
                &id,
                verdict,
                body.blocker_answer.as_deref().unwrap_or_default(),
                actor,
                &mut settled_ids,
            )
            .await?
        }
        None => match (body.verdict, body.amended_payload) {
            (Verdict::Approve, Some(payload)) => {
                runtime
                    .resolve_approval_amended_spawned(&id, payload, actor)
                    .await?
            }
            (Verdict::Deny, Some(_)) => {
                return Err(ApiError(OpenCompanyError::InvalidRequest(
                    "amended_payload cannot accompany a deny verdict".to_string(),
                )));
            }
            (verdict, None) => {
                runtime
                    .resolve_approval_spawned(&id, verdict, actor, scope)
                    .await?
            }
        },
    };

    // Read once, here: the verdict is durable and the follow-up cycle — which is
    // what decrements the turn's counter — has not run yet, so this still counts
    // the approval just decided and `decisions_still_awaited` subtracts it.
    let still_awaiting = runtime.decisions_still_awaited(&id);
    // Issue #1449: which end state this actually reached, read off the receipt
    // rather than assumed from the fact that no error was returned. A resolve
    // can succeed as a request and still not be the operator's decision.
    let outcome = receipt.outcome();

    if body.detach {
        // Nothing here waits on the turn. The webhook fan-out still owes the
        // report, so it moves onto its own task rather than being dropped —
        // a detached resolve must not silently stop notifying subscribers.
        let state = state.clone();
        let company = company.clone();
        tokio::spawn(async move {
            // A failed cycle already logged itself in `spawn_follow_up`; a
            // panicked one is worth its own line, since nothing else reports it.
            match crate::company::runtime::join_follow_up(follow_up).await {
                Ok(report) => emit_cycle_webhooks(&state, &company, &report).await,
                Err(OpenCompanyError::BackgroundTask(detail)) => {
                    tracing::error!(%company, %detail, "[approval] a detached follow-up cycle did not finish");
                }
                Err(_) => {}
            }
        });
        return Ok(Json(ResolveReceiptDto {
            recorded: true,
            already_resolved: receipt.already_resolved(),
            still_awaiting,
            outcome,
            settled_ids,
        })
        .into_response());
    }

    let report = crate::company::runtime::join_follow_up(follow_up).await?;
    emit_cycle_webhooks(state, company, &report).await;
    Ok(Json(ChatResponse {
        message_id: None,
        responses: readable_responses(report.responses),
        still_awaiting: Some(still_awaiting),
        outcome: Some(outcome),
        review_feedback_applied: None,
        settled_ids: (!settled_ids.is_empty()).then_some(settled_ids),
        // A resolve runs a follow-up cycle, not an operator turn, so it opens no
        // turn row of its own.
        turn_id: None,
    })
    .into_response())
}

/// The approval a resolve or an extend addresses, under either scope form.
///
/// Named rather than positional because the two forms carry different path
/// tuples — `{id}` plus `{aid}`, or `{aid}` alone — and a named capture
/// deserializes identically from both. The company is not read here:
/// [`AdminScopedCompany`] has already resolved and authorized it.
#[derive(Debug, Deserialize)]
struct ApprovalPath {
    aid: String,
}

/// `POST {scope}/approvals/{aid}` — decide a parked approval.
async fn resolve_approval(
    admin: AdminScopedCompany,
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(ApprovalPath { aid }): Path<ApprovalPath>,
    Json(body): Json<ResolveApproval>,
) -> Result<Response, crate::server::Rejection> {
    let company = admin.id().clone();
    let actor = resolving_actor(auth);
    run_resolve(&state, &company, admin.runtime, aid, body, actor)
        .await
        .map_err(|error| IntoResponse::into_response(error).into())
}

/// Who is resolving this approval (issue #374).
///
/// Both resolve handlers used to hardcode `Actor { kind: Operator, id:
/// "operator" }` while already holding the authenticated principal — so the
/// journal recorded every verdict as having come from the same anonymous
/// "operator", on a multi-user company where several people can decide. That was
/// tolerable while the record was one verdict; a standing grant is a permission
/// that outlives the decision and that someone else will later find and have to
/// account for, so "who opened this up" has to be a real answer.
fn resolving_actor(auth: GqlAuth) -> Actor {
    match auth {
        GqlAuth::User(user) => Actor {
            kind: ActorKind::User,
            id: user.user_id,
        },
        GqlAuth::Platform(_) => platform_actor(),
    }
}

/// The answer to an extend: the approval's new deadline, so the console can
/// redraw the countdown without re-fetching the whole approvals list.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtendReceiptDto {
    /// Always `true` — a failure is an error response instead. Present so the
    /// body is self-describing rather than an empty object.
    extended: bool,
    /// The approval's new default-deny instant (epoch-millis), the extension
    /// time plus the gate's current TTL — the same number the card now projects.
    expires_at_millis: f64,
}

async fn run_extend(
    runtime: Arc<CompanyRuntime>,
    approval_id: String,
    actor: Actor,
) -> Result<Response, ApiError> {
    runtime.ensure_running().await?;
    let id = ApprovalId::new(approval_id);
    // `extend_approval` refuses an unknown/already-decided id with `NotFound`,
    // which maps to 404 — so an operator extending something that has since
    // resolved or expired is told, not silently answered 200.
    let expires_at_millis = runtime.extend_approval(&id, actor).await?;
    Ok(Json(ExtendReceiptDto {
        extended: true,
        expires_at_millis: expires_at_millis as f64,
    })
    .into_response())
}

/// `POST {scope}/approvals/{aid}/extend` — push the default-deny deadline out
/// (issue #1805).
async fn extend_approval(
    admin: AdminScopedCompany,
    CompanyAuth(auth): CompanyAuth,
    Path(ApprovalPath { aid }): Path<ApprovalPath>,
) -> Result<Response, crate::server::Rejection> {
    let actor = resolving_actor(auth);
    run_extend(admin.runtime, aid, actor)
        .await
        .map_err(|error| IntoResponse::into_response(error).into())
}

#[cfg(test)]
#[path = "operator_aside_dto_tests.rs"]
mod operator_aside_dto_tests;
#[cfg(test)]
#[path = "operator_test_group_1.rs"]
mod operator_test_group_1;
#[cfg(test)]
#[path = "operator_test_group_10.rs"]
mod operator_test_group_10;
#[cfg(test)]
#[path = "operator_test_group_11.rs"]
mod operator_test_group_11;
#[cfg(test)]
#[path = "operator_test_group_12.rs"]
mod operator_test_group_12;
#[cfg(test)]
#[path = "operator_test_group_13.rs"]
mod operator_test_group_13;
#[cfg(test)]
#[path = "operator_test_group_14.rs"]
mod operator_test_group_14;
#[cfg(test)]
#[path = "operator_test_group_15.rs"]
mod operator_test_group_15;
#[cfg(test)]
#[path = "operator_test_group_16.rs"]
mod operator_test_group_16;
#[cfg(test)]
#[path = "operator_test_group_17.rs"]
mod operator_test_group_17;
#[cfg(test)]
#[path = "operator_test_group_18.rs"]
mod operator_test_group_18;
#[cfg(test)]
#[path = "operator_test_group_2.rs"]
mod operator_test_group_2;
#[cfg(test)]
#[path = "operator_test_group_3.rs"]
mod operator_test_group_3;
#[cfg(test)]
#[path = "operator_test_group_4.rs"]
mod operator_test_group_4;
#[cfg(test)]
#[path = "operator_test_group_5.rs"]
mod operator_test_group_5;
#[cfg(test)]
#[path = "operator_test_group_6.rs"]
mod operator_test_group_6;
#[cfg(test)]
#[path = "operator_test_group_7.rs"]
mod operator_test_group_7;
#[cfg(test)]
#[path = "operator_test_group_8.rs"]
mod operator_test_group_8;
#[cfg(test)]
#[path = "operator_test_group_9.rs"]
mod operator_test_group_9;
#[cfg(test)]
#[path = "operator_test_support_1.rs"]
mod operator_test_support_1;
#[cfg(test)]
#[path = "operator_test_support_2.rs"]
mod operator_test_support_2;
#[cfg(test)]
#[path = "operator_test_support_3.rs"]
mod operator_test_support_3;
#[cfg(test)]
#[path = "operator_test_support_4.rs"]
mod operator_test_support_4;
