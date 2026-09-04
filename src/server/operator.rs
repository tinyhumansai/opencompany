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
use crate::ports::events::EventStreamItem;
use crate::ports::store::company_write_lock;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, Attachment, CompanyEvent, CompanyId, CompanyRecord, EventSeq,
    OutboundMessage, OverlayDesk, OverlayDeskMember, OverlayDeskOrder, ResponderMode, StoredEvent,
    TurnStep, Verdict,
};
use crate::runtime::grants::{GrantId, GrantScope, MAX_STANDING_GRANT_MILLIS};
use crate::runtime::types::{ApprovalSummary, CompanyStatus, CycleReport};
use crate::server::chat_history::{
    CHAT_HISTORY_PAGE_LIMIT, MentionView, MessageView, ReactionView, Viewer, author_labels,
    channel_attributed_replies, history_for_desk, project_mentions,
};
use crate::server::error::ApiError;
use crate::server::graphql::auth::GqlAuth;
use crate::server::ops::language::{self, DEFAULT_DESK};
use crate::server::ops::{ScopedCompany, scoped};
use crate::server::platform_auth::{CompanyAuth, authorize_address, refuse_until_password_changed};
use crate::server::provision::{emit_cycle_webhooks, emit_feedback_webhook};

/// Builds the operator route fragment, merged into the main router.
pub fn router() -> Router<AppState> {
    let router = Router::new()
        .route("/api/v1/companies", get(list_companies))
        .route("/api/v1/companies/{id}", get(company_status))
        .route("/api/v1/companies/{id}/chat", post(operator_chat))
        .route("/api/v1/companies/{id}/chat/history", get(chat_history))
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
        .route(
            "/api/v1/companies/{id}/approvals/{aid}",
            post(resolve_approval),
        )
        .route(
            "/api/v1/companies/{id}/approvals/{aid}/extend",
            post(extend_approval),
        )
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
        .route(
            "/api/v1/company/approvals/{aid}/extend",
            post(extend_approval_single),
        )
        .route(
            "/api/v1/company/approvals/{aid}",
            post(resolve_approval_single),
        )
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
            let manifest_desks = record.manifest.group_chats.iter().map(|chat| {
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
        desk_id,
        agent_id: body.agent_id,
    });
    scope.runtime.store().save(&record).await?;
    Ok(StatusCode::NO_CONTENT)
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
    };
    record.overlay_desks.push(desk);
    scope.runtime.store().save(&record).await?;

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
    scope.runtime.store().save(&record).await?;
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
            o["text"] = json!(text);
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
        CompanyEvent::TaskDispatched { task_id, .. } => {
            let mut o = envelope("task_dispatched");
            o["taskId"] = json!(task_id);
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
    // Deterministic task card, opened only for a message the triage calls
    // `Track`: an actionable operator request ("build the landing page", "can
    // you set up the newsletter") opens a `todo` card so "do X" always leaves a
    // visible work item on the dashboard — independent of whether the
    // orchestrator model also calls `spawn_task` (it may open sub-tasks on top).
    // Best-effort: a card write failure must never sink the chat reply.
    //
    // Issue #267 turned the boolean this used to read into a positive three-way
    // classification. `Answer` (a question about state, a read request) and
    // `Chatter` (greetings, acknowledgements, anything ambiguous) both open
    // nothing here — but they are NOT the same answer, and the difference
    // matters one layer down: `Answer` additionally takes the model's own
    // board-writing tools away for the turn, in
    // `DelegationRunner::handle_operator_message`, because a question was the
    // other door dead cards came through. This site is Layer A of that pair: it
    // is compiled into every build and fronts both cognition brains, whereas the
    // gate is on the harness path only.
    //
    // NOT on a workflow copilot thread (issue #416). A copilot question is
    // phrased at the workflow — "add a node that emails the report", "why does
    // this fail on Mondays" — and the triage reads the first of those as a
    // request to the company, which would put a card on the board from a
    // conversation the operator was having *about a graph*. The confinement in
    // the harness stops the turn from acting; this stops the route from acting
    // on its behalf, and it holds in every build because it is here rather than
    // behind the `openhuman` feature.
    //
    // Issue #845: an explicit `workflow` deliverable opens a card whatever the
    // triage said. The operator reached for a control **named** "Build me the
    // workflow" and pressed it — that is a positive statement of intent about
    // this message, and it is better evidence than a lexical classifier's guess.
    // Where the two disagreed, the classifier won and the choice was dropped on
    // the floor: no card, so no builder pass, so nothing built, and no error
    // either — the operator got a conversational reply to a request they had
    // asked to be turned into a workflow.
    //
    // Deliberately narrow. It does not change what the card *is* (the title
    // still comes from the triage, or from the message when the triage declined
    // to name one), it does not touch `Track`'s existing behaviour, and it stays
    // inside the `!confined` guard — a copilot thread still opens nothing,
    // because a message *about* a graph is not a request to build one.
    let workflow_requested =
        !confined && message.deliverable == Some(crate::ports::types::MessageIntent::Workflow);
    // Issue #1152: and the other direction — the operator said this message is
    // NOT a request for work ("Just chatting"), so no deterministic path may
    // card it whatever the triage read in the words.
    //
    // The operator could already *mint* a card the classifier declined
    // (`workflow_requested`, above) and could never *withhold* one. That
    // asymmetry is the bug: a message the triage happens to read as `Track` —
    // "we should probably rewrite the pricing page some day" — opened a card,
    // assigned it to a desk, and there was no control anywhere that said
    // otherwise. This is that control, and it is the same kind of evidence the
    // override above is: a positive statement by the person who wrote the
    // message, which is better than a lexical guess about it.
    //
    // **No `!confined` term, deliberately.** `workflow_requested` needs one
    // because it *mints* a card, and minting one on a workflow copilot thread —
    // a conversation ABOUT one graph — is exactly what #416 suppresses. This
    // only ever *subtracts*, and on a copilot thread the branch below is already
    // suppressed, so a `!confined` term here would be inert at best and would
    // read as though the two were symmetrical.
    let not_work = message
        .deliverable
        .is_some_and(crate::ports::types::MessageIntent::is_chat);
    // The lexical layer answers two questions at once, and only the first is a
    // decision: whether this message becomes a card, and — for a host with no
    // model wired — what to call it. The second is now a fallback. Keeping it
    // matters: the classifier returns a *tidied* title, so discarding it would
    // make an offline company's cards worse than before rather than no better.
    let lexical = (!confined && !not_work)
        .then(|| crate::company::task_intent::triage_message(&message.text))
        .and_then(|triage| match triage {
            crate::company::task_intent::MessageTriage::Track(title) => Some(title),
            crate::company::task_intent::MessageTriage::Answer
            | crate::company::task_intent::MessageTriage::Chatter => None,
        })
        .or_else(|| {
            workflow_requested.then(|| crate::company::task_intent::to_title(message.text.trim()))
        })
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
        mentions: runtime
            .resolve_mentions(&message.text, message.mentions.clone(), by)
            .await,
        // Issue #1682: the store-resolved references, so the durable record
        // carries the name/mime/size the store computed and never the client's
        // claim. Empty on a message with no attachment, which skips the field.
        attachments,
    };
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

    let turn_id = crate::ports::generate_id();
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
            crate::ports::runs::NewRun::for_chat(turn_id.clone(), desk, desk).in_thread(parent),
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
        let _serialized = runtime.task_writes.lock().await;
        match runtime
            .plan_blocker_reply(&desk, parent, &message.text)
            .await?
        {
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
                })));
            }
            crate::company::runtime::BlockerReplyPlan::AskWhich { prompt } => {
                let accepted =
                    accept_chat_turn(&runtime, id, &message, by.as_ref(), parent, &desk).await?;
                let message_id = accepted.message_seq.value().to_string();
                let turn_id = accepted.turn_id.clone();
                let posted = runtime
                    .post_blocker_prompt(&desk, &prompt)
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
    let responses = report.responses.clone();
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
mod turn_failure_notice_tests {
    use super::{provider_failure_sentence, turn_failure_notice};

    /// The failure that put a wall of provider JSON into company chat, verbatim
    /// from the 1/9 round (issue #2016). None of it may reach the operator, and
    /// what they read instead has to say whether waiting will help.
    #[cfg(feature = "openhuman")]
    #[test]
    fn a_rate_limit_reaches_the_operator_as_a_sentence_not_a_payload() {
        let raw = concat!(
            "turn for 'frontend_engineer': inference returned 429 Too Many Requests: ",
            r#"{"error":{"message":"Provider returned error","code":429,"metadata":"#,
            r#"{"raw":"deepseek/deepseek-chat is temporarily rate-limited upstream. "#,
            r#"Please retry shortly, or add your own key to accumulate your rate limits: "#,
            r#"https://openrouter.ai/settings/integrations","provider_name":"DeepInfra"}}}"#,
        );
        let notice = turn_failure_notice(raw);

        assert!(notice.contains("rate-limiting"), "{notice}");
        for leaked in ["{", "openrouter.ai", "deepseek", "DeepInfra", "429"] {
            assert!(
                !notice.contains(leaked),
                "the provider payload leaked {leaked:?} into chat: {notice}"
            );
        }
    }

    /// An empty inference is its own message: the harness has already retried it
    /// by the time this is written, so leading with "try again" is wrong advice
    /// and the cause is worth naming.
    #[test]
    fn an_empty_inference_says_so() {
        let notice = turn_failure_notice(concat!(
            "inference response carried neither choices[0].message.content nor tool_calls ",
            "(finish_reason: failed; choices: 1; usage: in=0 out=0 total=0)"
        ));

        assert!(notice.contains("empty response"), "{notice}");
        assert!(
            !notice.contains("finish_reason"),
            "diagnostics leaked into chat: {notice}"
        );
    }

    /// Quota exhaustion is not wait-and-retry — somebody has to go and fix the
    /// account — so it must not be worded like a transient blip.
    #[cfg(feature = "openhuman")]
    #[test]
    fn quota_exhaustion_points_at_the_account() {
        let notice = turn_failure_notice(concat!(
            "inference returned 429 Too Many Requests: ",
            r#"{"error":{"message":"insufficient balance"}}"#
        ));

        assert!(notice.contains("quota or credit"), "{notice}");
        assert!(notice.contains("Settings"), "{notice}");
    }

    /// A status our own error format hides from `structured_http_status`. With
    /// it invisible, a 402 fell through to the `Retryable` default and was
    /// reported as "temporarily unavailable" — telling an operator to wait for
    /// something that will never clear on its own.
    #[cfg(feature = "openhuman")]
    #[test]
    fn a_status_only_our_own_prefix_carries_is_still_classified() {
        let notice = turn_failure_notice("inference returned 402 Payment Required: no credit");

        assert!(notice.contains("rejected the request"), "{notice}");
    }

    /// The guard against over-claiming. `classify_provider_failure` falls back
    /// to `Retryable` for text it recognizes nothing in, so a tool that ran out
    /// of wall-clock would otherwise be reported as a provider outage.
    #[test]
    fn a_failure_that_is_not_the_providers_is_not_blamed_on_it() {
        let raw = "the tool call exceeded its wall-clock budget";

        assert!(
            provider_failure_sentence(raw).is_none(),
            "a non-provider failure must not be classified as one"
        );
        let notice = turn_failure_notice(raw);
        assert!(notice.contains("something went wrong"), "{notice}");
        assert!(!notice.contains("provider"), "{notice}");
    }

    /// Whatever the cause, the operator is told the turn left nothing behind —
    /// the one fact they need in order to decide whether to re-send.
    #[test]
    fn every_notice_states_that_nothing_was_half_done() {
        for raw in [
            "inference returned 429 Too Many Requests: rate limited",
            "inference response carried neither choices[0].message.content nor tool_calls",
            "something else entirely",
        ] {
            assert!(
                turn_failure_notice(raw).contains("Nothing was left half-done"),
                "missing for: {raw}"
            );
        }
    }
}

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
                let notice = CompanyEvent::AgentReply {
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
                    text: turn_failure_notice(&err.0.to_string()),
                    steps: Vec::new(),
                    task_id: None,
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
        settle_chat_turn(&runtime, &company, turn_id.as_deref(), None).await;
        Ok((report, feedback_note))
    })
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
                    // Who this reply names. Rendered as chips and — unlike an
                    // operator message's — never consulted by dispatch, which
                    // is the mention-loop fuse.
                    mentions: reply_mentions.clone(),
                    // Zero, and stays zero while that edge does not exist.
                    mention_depth: 0,
                    // The answer joins the thread its question was asked in,
                    // rather than opening one under the question (issue #364).
                    parent,
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
                    chat_id: desk.to_string(),
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
                // The durable half of a reply's mention, same as an operator
                // message's (issue: mentions). Without this an `@user` an agent
                // types back renders as a chip and nothing else — the badge and
                // the notification both silently missing for whoever it named,
                // which is worst for exactly the person it is meant to reach:
                // offline when the reply lands.
                if !reply_mentions.is_empty() {
                    runtime
                        .notify_mentions(id, &reply_mentions, &seq, None, desk)
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
        Self {
            id: view.id,
            channel: view.channel,
            author: view.author,
            text: view.text,
            at_millis: view.at_millis,
            mine: view.mine,
            by_person: view.by_person,
            steps: view.steps,
            task_id: view.task_id,
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
async fn revoke_grant(
    scope: ScopedCompany,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<StatusCode, ApiError> {
    let gid = params
        .get("gid")
        .cloned()
        .ok_or_else(|| ApiError(OpenCompanyError::InvalidRequest("missing grant id".into())))?;
    // The machine credential has no person behind it, the same distinction every
    // other operator write draws.
    let by = scope.actor.clone().unwrap_or_else(platform_actor);
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
    // leaves the approval parked with no verdict journaled.
    let scope = grant_scope(&body)?;
    let id = ApprovalId::new(approval_id);
    // The verdict is settled inline; only the follow-up cycle is on the handle.
    // So by the time this returns — in either mode — the decision is journaled
    // and any grant is minted.
    let (receipt, follow_up) = match (body.verdict, body.amended_payload) {
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
        })
        .into_response());
    }

    let report = crate::company::runtime::join_follow_up(follow_up).await?;
    emit_cycle_webhooks(state, company, &report).await;
    Ok(Json(ChatResponse {
        message_id: None,
        responses: report.responses,
        still_awaiting: Some(still_awaiting),
        outcome: Some(outcome),
        review_feedback_applied: None,
        // A resolve runs a follow-up cycle, not an operator turn, so it opens no
        // turn row of its own.
        turn_id: None,
    })
    .into_response())
}

/// `POST /api/v1/companies/{id}/approvals/{aid}`.
async fn resolve_approval(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, String)>,
    Json(body): Json<ResolveApproval>,
) -> Result<Response, crate::server::Rejection> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp.into());
    }
    let runtime = lookup(&state, &id)?;
    let actor = resolving_actor(auth);
    run_resolve(&state, &company, runtime, aid, body, actor)
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

/// `POST /api/v1/company/approvals/{aid}` (single-company alias).
async fn resolve_approval_single(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(aid): Path<String>,
    Json(body): Json<ResolveApproval>,
) -> Result<Response, crate::server::Rejection> {
    let runtime = sole(&state)?;
    let id = runtime.id().clone();
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return Err(resp.into());
    }
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return Err(resp.into());
    }
    let actor = resolving_actor(auth);
    run_resolve(&state, &id, runtime, aid, body, actor)
        .await
        .map_err(|error| IntoResponse::into_response(error).into())
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

/// `POST /api/v1/companies/{id}/approvals/{aid}/extend` (issue #1805).
async fn extend_approval(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, String)>,
) -> Result<Response, crate::server::Rejection> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp.into());
    }
    let runtime = lookup(&state, &id)?;
    let actor = resolving_actor(auth);
    run_extend(runtime, aid, actor)
        .await
        .map_err(|error| IntoResponse::into_response(error).into())
}

/// `POST /api/v1/company/approvals/{aid}/extend` (single-company alias).
async fn extend_approval_single(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(aid): Path<String>,
) -> Result<Response, crate::server::Rejection> {
    let runtime = sole(&state)?;
    let id = runtime.id().clone();
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return Err(resp.into());
    }
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return Err(resp.into());
    }
    let actor = resolving_actor(auth);
    run_extend(runtime, aid, actor)
        .await
        .map_err(|error| IntoResponse::into_response(error).into())
}

#[cfg(test)]
mod test {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::tasks::TaskTitle;
    use crate::ports::types::CompanyRecord;
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-http-")
            .tempdir()
            .expect("tempdir")
    }

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
    }

    async fn state_with_company(home: &std::path::Path, lifecycle: &str) -> AppState {
        build_state(home, lifecycle, AppConfig::default()).await
    }

    async fn build_state(home: &std::path::Path, lifecycle: &str, config: AppConfig) -> AppState {
        build_state_with_brain(home, lifecycle, config, None).await
    }

    /// [`build_state`], optionally swapping the runtime's cognition. The
    /// approval-continuation tests need a brain they can stall mid-turn.
    async fn build_state_with_brain(
        home: &std::path::Path,
        lifecycle: &str,
        config: AppConfig,
        brain: Option<Arc<dyn crate::ports::brain::Brain>>,
    ) -> AppState {
        build_state_with_brain_and_manifest(home, lifecycle, config, brain, manifest()).await
    }

    /// [`build_state_with_brain`], with the company manifest chosen by the
    /// caller — the approval **deadline** lives in `[policy]`, so a test about
    /// what a past-deadline card answers has to be able to set it (issue #1449).
    async fn build_state_with_brain_and_manifest(
        home: &std::path::Path,
        lifecycle: &str,
        config: AppConfig,
        brain: Option<Arc<dyn crate::ports::brain::Brain>>,
        manifest: CompanyManifest,
    ) -> AppState {
        // Pre-seed a record so the builder preserves the requested lifecycle.
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        use crate::ports::CompanyStore;
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: lifecycle.to_string(),
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
        if let Some(brain) = brain {
            builder = builder.with_brain(brain);
        }
        let runtime = builder.build().await.unwrap();
        let state = AppState::new(config);
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    /// A run store that refuses every verb — the persistence layer mid-outage.
    /// `accept_chat_turn` treats a refused row best-effort, so this store is
    /// what probes the other half of that promise: the turn still runs and the
    /// request still gets an answer, it just cannot be a pollable `202`.
    struct FailingRunStore;

    #[async_trait::async_trait]
    impl crate::ports::runs::RunStore for FailingRunStore {
        async fn create_run(
            &self,
            _company: &CompanyId,
            _spec: crate::ports::runs::NewRun,
        ) -> crate::Result<crate::ports::runs::RunRecord> {
            Err(OpenCompanyError::InvalidRequest(
                "run store offline".to_string(),
            ))
        }
        async fn get_run(
            &self,
            _company: &CompanyId,
            _id: &str,
        ) -> crate::Result<Option<crate::ports::runs::RunRecord>> {
            Ok(None)
        }
        async fn put_run(
            &self,
            _company: &CompanyId,
            _run: &crate::ports::runs::RunRecord,
        ) -> crate::Result<()> {
            Err(OpenCompanyError::InvalidRequest(
                "run store offline".to_string(),
            ))
        }
        async fn list_runs(
            &self,
            _company: &CompanyId,
            _filter: &crate::ports::runs::RunFilter,
        ) -> crate::Result<Vec<crate::ports::runs::RunRecord>> {
            Ok(Vec::new())
        }
        async fn append_run_step(
            &self,
            _company: &CompanyId,
            _step: &crate::ports::runs::RunStepRecord,
        ) -> crate::Result<()> {
            Err(OpenCompanyError::InvalidRequest(
                "run store offline".to_string(),
            ))
        }
        async fn list_run_steps(
            &self,
            _company: &CompanyId,
            _run_id: &str,
        ) -> crate::Result<Vec<crate::ports::runs::RunStepRecord>> {
            Ok(Vec::new())
        }
    }

    /// [`state_with_company`] with the run store swapped for one that refuses
    /// every verb — the setup for the rowless-turn tests.
    async fn state_with_failing_runs(home: &std::path::Path) -> AppState {
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        use crate::ports::CompanyStore;
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: manifest(),
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
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .with_runs(Arc::new(FailingRunStore))
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    #[tokio::test]
    async fn chat_returns_echoed_response() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    // Issue #1725: not "hi". A bare pleasantry is answered by
                    // the runtime without a turn, so the echo brain — which is
                    // what this asserts is wired up — never sees it.
                    .body(Body::from(r#"{"text":"ship the landing page"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["responses"][0]["text"],
            "You said: ship the landing page"
        );
        assert_eq!(value["responses"][0]["channel"], "operator");
    }

    /// An actionable operator chat opens exactly one task card on the dashboard
    /// (deterministic, independent of the brain's own `spawn_task`), and a
    /// greeting opens none. Runs on the default echo brain, so it proves the
    /// handler-level wiring, not model behaviour.
    ///
    /// Issue #576: that card now lands in **Planning**, not To-do. The request
    /// carries `fixed_cookie`, so a signed-in person is behind it — which is
    /// what the promotion is conditional on.
    ///
    /// `tasks.len() == 1` is doing real work here beyond "a card was opened":
    /// the card is created *directly* in `planning` by a single `upsert_task`,
    /// so a second card, or a card that arrived via To-do and was promoted,
    /// would both show up here.
    #[tokio::test]
    async fn actionable_chat_opens_a_planning_task_card() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        let chat = |text: &str| {
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"text":{}}}"#,
                    serde_json::json!(text)
                )))
                .unwrap()
        };

        // Actionable → one Planning card, titled from the ask.
        let r = app
            .clone()
            .oneshot(chat("build the landing page"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1, "an actionable ask opens one card");
        assert_eq!(
            tasks[0].column,
            crate::ports::tasks::COLUMN_PLANNING,
            "issue #576: the prompt box promotes its own card, with no drag"
        );
        assert_eq!(tasks[0].priority, "medium");
        assert_eq!(tasks[0].title, "Build the landing page");

        // Greeting → no new card.
        let r = app.oneshot(chat("thanks!")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1, "a greeting must not open a card");
    }

    /// Issue #1725, through the route an operator actually hits: "hi" comes
    /// back answered, with no card, no steps, and no turn behind it.
    ///
    /// The unit-level proof that the brain is not called lives in
    /// `runtime::cycle`'s `a_bare_greeting_answers_without_calling_the_brain`,
    /// where a counting brain can be injected. This one pins that the chat
    /// handler reaches that path at all — the two are separate failures, and a
    /// correct fast path nothing routes to leaves the bug where it was.
    ///
    /// The echo brain answers `"You said: <text>"`, so the assertion below is
    /// also the evidence: a canned greeting means the brain never ran.
    #[tokio::test]
    async fn a_bare_greeting_is_answered_without_a_turn() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["responses"][0]["text"],
            crate::company::task_intent::SmallTalk::Hello.reply(),
            "a greeting is answered by the runtime, not by a turn"
        );
        // The console showed "1 step" for a greeting on staging. There is no
        // step to show, so the field is omitted entirely.
        assert!(
            value["responses"][0]["steps"].is_null(),
            "no tool ran: {}",
            value["responses"][0]
        );
        assert!(
            runtime.tasks().list(&id).await.unwrap().is_empty(),
            "a greeting opens no card"
        );
    }

    // ── Issue #982: the card goes to whoever was addressed ──────────────────

    /// A roster with three teammates and one desk, so a chat can be addressed
    /// to something that exists.
    ///
    /// The ids are the ones the smoke that found #982 used, and the roles are
    /// deliberately distinct words, so a test can address one teammate in a
    /// message whose text points at another.
    fn roster_manifest() -> CompanyManifest {
        toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "product_manager"
role = "Product Manager"

[[agent]]
id = "backend_engineer"
role = "Backend Engineer"

[[agent]]
id = "designer"
role = "Designer"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = ["backend_engineer"]

[policy]
mode = "full"
"#,
        )
        .unwrap()
    }

    /// [`state_with_company`] over [`roster_manifest`]. Written out rather than
    /// threaded through the shared builders above, which several other suites
    /// call with the roster-less fixture.
    async fn state_with_roster(home: &std::path::Path) -> AppState {
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        use crate::ports::CompanyStore;
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: roster_manifest(),
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
        let runtime = RuntimeBuilder::new(home.to_path_buf(), roster_manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    /// [`roster_manifest`] plus a **memberless** desk — one that exists on the
    /// roster but has nobody seated on it, the `EmptyDesk` shape `mention_context`
    /// still has to canonicalize.
    fn memberless_desk_manifest() -> CompanyManifest {
        toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "product_manager"
role = "Product Manager"

[[agent]]
id = "backend_engineer"
role = "Backend Engineer"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = ["backend_engineer"]

[[group_chat]]
id = "sales"
name = "Sales"
members = []

[policy]
mode = "full"
"#,
        )
        .unwrap()
    }

    /// [`roster_manifest`] plus a desk **literally named** `dm:engineering`,
    /// beside the ordinary `engineering` desk — the shape `mention_context`
    /// must resolve **as sent** instead of stripping the `dm:` prefix away.
    fn dm_prefixed_desk_manifest() -> CompanyManifest {
        toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "product_manager"
role = "Product Manager"

[[agent]]
id = "backend_engineer"
role = "Backend Engineer"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = ["backend_engineer"]

[[group_chat]]
id = "dm:engineering"
name = "Dm Engineering"
members = ["backend_engineer"]

[policy]
mode = "full"
"#,
        )
        .unwrap()
    }

    /// [`state_with_roster`] over [`memberless_desk_manifest`].
    async fn state_with_memberless_desk(home: &std::path::Path) -> AppState {
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        use crate::ports::CompanyStore;
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: memberless_desk_manifest(),
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
        let runtime = RuntimeBuilder::new(home.to_path_buf(), memberless_desk_manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    /// [`state_with_roster`] over [`dm_prefixed_desk_manifest`].
    async fn state_with_dm_prefixed_desk(home: &std::path::Path) -> AppState {
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        use crate::ports::CompanyStore;
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: dm_prefixed_desk_manifest(),
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
        let runtime = RuntimeBuilder::new(home.to_path_buf(), dm_prefixed_desk_manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    /// One chat request, optionally addressed to a thread.
    fn chat_to(text: &str, chat: Option<&str>) -> Request<Body> {
        chat_in_thread(text, chat, None)
    }

    /// The same send, typed inside a thread — `parent` is the root the console
    /// sends when the operator answers in an open thread (#1890 B).
    fn chat_in_thread(text: &str, chat: Option<&str>, parent: Option<u64>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/company/chat")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "text": text,
                    "chat": chat,
                    // A string, like every other message id on this API — the
                    // field's own note says so, and a number is a 422.
                    "parent": parent.map(|seq| seq.to_string()),
                })
                .to_string(),
            ))
            .unwrap()
    }

    /// The message the smoke sent: an actionable ask whose *text* points at one
    /// teammate, addressed to a different one.
    const CROSSED: &str = "build the backend deployment pipeline";

    /// The card a chat opens is handed to the teammate the operator addressed.
    ///
    /// The fixture is the whole test. `CROSSED` names *backend* work and is
    /// addressed to the **product manager**, so the two candidate answers are
    /// distinguishable: pre-fix this card was born blank and the planning pass
    /// filled it from a content match of the title against teammate roles —
    /// which is exactly the wrong answer here. A message whose text and
    /// addressee agree would pass on pre-fix code and prove nothing; that is
    /// what the two "right" rows of the issue's table were.
    #[tokio::test]
    async fn chat_addressed_to_a_teammate_assigns_that_teammate() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_roster(&home).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        assert!(
            matches!(
                crate::company::task_intent::triage_message(CROSSED),
                crate::company::task_intent::MessageTriage::Track(_)
            ),
            "fixture must be a message the handler cards, or this proves nothing"
        );

        let r = app
            .oneshot(chat_to(CROSSED, Some("product_manager")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1, "an actionable ask opens one card");
        assert_eq!(
            tasks[0].assignee, "product_manager",
            "the card belongs to the teammate the operator addressed"
        );
        assert_ne!(
            tasks[0].assignee, "backend_engineer",
            "…and not to whoever the message text happens to name"
        );
    }

    /// A desk-addressed chat is assigned to the **desk**, not to its lead.
    ///
    /// Writing the lead would erase the desk from the board the moment the card
    /// was created — the invariant `AssigneeResolution::canonical` holds for
    /// every other write site (issue #214), now held here too.
    #[tokio::test]
    async fn chat_addressed_to_a_desk_assigns_the_desk() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_roster(&home).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        let r = app
            .oneshot(chat_to(CROSSED, Some("engineering")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].assignee, "engineering",
            "picking a desk IS the operator's routing decision"
        );
    }

    /// Everything that addresses nobody in particular still opens a blank card:
    /// no thread at all, the empty string, the console's legacy fallback desk
    /// id, and the default "General" desk this company does not have.
    ///
    /// This pins the direction of the change — *more* cards are operator-chosen,
    /// none fewer — and it is the clause that keeps the orchestrator's own queue
    /// working: a blank assignee is what hands a card to it.
    #[tokio::test]
    async fn an_unaddressed_chat_leaves_the_card_unassigned() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_roster(&home).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        for thread in [None, Some(""), Some("main"), Some(DEFAULT_DESK)] {
            let r = app.clone().oneshot(chat_to(CROSSED, thread)).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK, "thread {thread:?}");
        }

        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 4, "one card per message: {tasks:?}");
        for card in &tasks {
            assert_eq!(
                card.assignee, "",
                "an unaddressed message leaves the card for the orchestrator"
            );
        }
    }

    /// A thread key that names nothing on the roster is not an error: the card
    /// is opened, unassigned, exactly as it was before this route resolved
    /// anything. A chat must never 400 — and must never lose its card — over who
    /// it was addressed to.
    #[tokio::test]
    async fn an_unknown_addressee_leaves_the_card_unassigned() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_roster(&home).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        let r = app
            .oneshot(chat_to(CROSSED, Some("nobody_by_that_name")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "an unknown thread is not a 400");

        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1, "…and the card is still opened");
        assert_eq!(tasks[0].assignee, "", "…with nobody guessed onto it");
    }

    /// The card remembers the thread it was opened from, so the marker that says
    /// it settled lands back in the conversation that asked for the work.
    ///
    /// `origin_chat_id` is the field issue #151 added for exactly this, and the
    /// console already renders the marker in whatever channel it names — the
    /// route was simply never filling it in. An unaddressed message still opens
    /// a card with no origin, which is every card this route opened before.
    #[tokio::test]
    async fn a_chat_card_remembers_the_thread_it_was_opened_from() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_roster(&home).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        let r = app
            .clone()
            .oneshot(chat_to(CROSSED, Some("dm:designer")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(
            tasks[0].origin_chat_id(),
            Some("dm:designer"),
            "the thread as the console addressed it"
        );

        let r = app
            .oneshot(chat_to("draft the investor update", None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        let unaddressed = tasks
            .iter()
            .find(|c| c.title == "Draft the investor update")
            .expect("the second card");
        // No desk, therefore no conversation and no thread inside one. Before
        // #1890 step 5 this card carried a thread root beside no desk — the
        // drifted pair — and the root was inert: `relay_reply` posts back
        // through the desk, so a root with nothing to post into named nothing.
        // `TaskOrigin` cannot hold that state, so it is simply absent now.
        //
        // Restoring a real origin here means stamping the General desk the
        // route already folds this message into, which is a behaviour change
        // and not this one.
        assert_eq!(
            unaddressed.origin_chat_id(),
            None,
            "an unaddressed message has no conversation to answer in"
        );
        assert_eq!(
            unaddressed.origin_parent(),
            None,
            "and therefore no thread inside one either"
        );

        // The addressed card, found by title rather than by index: the two are
        // listed together from here on, and this assertion is about the one
        // that has a desk.
        let addressed = tasks
            .iter()
            .find(|c| c.origin_chat_id() == Some("dm:designer"))
            .expect("the addressed card");
        // Reversed once #1890 D landed alongside B, and the reversal is the
        // point. B alone read the message's own `parent`, so a card raised from
        // a channel-level question recorded no thread — right while a thread
        // was only ever something an operator opened by hand.
        //
        // D changed what a thread is: an answer parents to the message that
        // opened the exchange, so that question is a root. A card raised from
        // it belongs to the thread it just started, and recording `None` here
        // would put the settle marker in the channel while the answer to the
        // same message sat in a thread — the split B exists to prevent.
        assert!(
            addressed.origin_parent().is_some(),
            "a channel-level question is itself the thread its card was raised in",
        );
    }

    /// Issue #1890 D part 1: every answer threads under the message that
    /// opened the exchange.
    ///
    /// The two arms are the whole rule, and the second is the change: before
    /// it, an answer to an unthreaded question was journaled unparented, so the
    /// only threads that existed were ones an operator opened by hand.
    #[test]
    fn an_answer_threads_under_the_message_that_opened_the_exchange() {
        let message = EventSeq::new(41);
        // Not in a thread: the exchange becomes one, rooted at the question.
        assert_eq!(reply_thread(None, message), Some(message));
        // Already in one: the same root, so a follow-up does not open a thread
        // of its own — N messages in a thread is one topic, not N.
        let root = EventSeq::new(7);
        assert_eq!(reply_thread(Some(root), message), Some(root));
        // Never `None`: uniform is what keeps `parent` out of the hands of race
        // timing, since `parent` is permanent and presentation is not.
        assert!(reply_thread(None, message).is_some());
        assert!(reply_thread(Some(root), message).is_some());
    }

    /// Issue #1890 B: the card remembers **which thread** inside that channel.
    ///
    /// The channel alone was never enough — a channel holds any number of live
    /// threads, and a settle filed against the channel surfaces in none of
    /// them. A message's own `parent` IS its root (a reply is parented to its
    /// question's parent, never to the question), so the route reads it
    /// straight off the send with no walk.
    #[tokio::test]
    async fn a_chat_card_remembers_the_thread_inside_the_channel() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_roster(&home).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        let r = app
            .oneshot(chat_in_thread(CROSSED, Some("dm:designer"), Some(41)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks[0].origin_chat_id(), Some("dm:designer"));
        assert_eq!(
            tasks[0].origin_parent(),
            Some(crate::ports::types::EventSeq::new(41)),
            "the root the operator was answering in",
        );
    }

    /// The console mints a DM channel id as `dm:<teammate-id>`, and that form is
    /// documented as a valid channel key — so it has to address the teammate
    /// here as well as in the responder lookup.
    #[tokio::test]
    async fn a_console_dm_channel_id_addresses_the_teammate() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_roster(&home).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        let r = app
            .oneshot(chat_to(CROSSED, Some("dm:designer")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].assignee, "designer");
    }

    /// Issue #845: an explicit "Build me the workflow" opens a card even when
    /// the triage would have opened nothing.
    ///
    /// The composer's toggle was consulted *only* on the card-opening branch,
    /// and that branch is gated on the triage. So a `workflow` request the
    /// classifier read as a question or as chatter dropped the choice on the
    /// floor: no card, therefore no builder pass, therefore nothing built — and
    /// no error either, because a conversational reply came back as though the
    /// message had been handled.
    ///
    /// Both halves are pinned here: the same text opens nothing as a `once`
    /// message and opens a `workflow` card when the operator asked for one.
    #[tokio::test]
    async fn an_explicit_workflow_request_opens_a_card_the_triage_declined() {
        use crate::ports::tasks::TaskDeliverable;

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        // A question by construction — `is_question` fires on the wh-opener, so
        // the triage answers `Answer` and the card branch declines.
        let text = "what would a weekly AEO audit of the blog even look like?";
        assert!(
            matches!(
                crate::company::task_intent::triage_message(text),
                crate::company::task_intent::MessageTriage::Answer
            ),
            "fixture must be one the triage declines to card, or this proves nothing"
        );

        let chat = |deliverable: Option<&str>| {
            let body = match deliverable {
                Some(d) => format!(
                    r#"{{"text":{},"deliverable":"{d}"}}"#,
                    serde_json::json!(text)
                ),
                None => format!(r#"{{"text":{}}}"#, serde_json::json!(text)),
            };
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };

        // `once` (and no choice at all): unchanged — the triage still decides.
        let r = app.clone().oneshot(chat(None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app.clone().oneshot(chat(Some("once"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(
            runtime.tasks().list(&id).await.unwrap().is_empty(),
            "a `once` question must still open nothing"
        );

        // `workflow`: the operator's explicit choice outranks the classifier.
        let r = app.oneshot(chat(Some("workflow"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1, "the workflow choice must open its card");
        assert_eq!(
            tasks[0].deliverable,
            TaskDeliverable::Workflow,
            "and it must be the deliverable that routes it to the builder pass"
        );
        // Titled through `to_title`, exactly as a `Track` card would have been.
        assert_eq!(
            tasks[0].title,
            crate::company::task_intent::to_title(text),
            "a bypassed card must be titled byte-for-byte as a tracked one"
        );
    }

    /// Issue #1152: an explicit "Just chatting" **withholds** the card the
    /// triage would otherwise have opened.
    ///
    /// The mirror of the test above, and the asymmetry it closes. Since #845 the
    /// operator could override the classifier *upward* — mint a card it
    /// declined — and there was no control anywhere that overrode it downward.
    /// So a message the lexical layer reads as `Track` ("can you build the
    /// landing page?" asked rhetorically, while thinking out loud) opened a
    /// card, assigned it to a desk, and started a planning pass, and the only
    /// recourse was to go to the board and delete it.
    ///
    /// The fixture's verdict is asserted `Track` **first**, in the strongest
    /// direction available: the `chat` run is made before any other, on an empty
    /// board, and the unmarked run right after it opens the card on the very
    /// same words. So "zero cards" is the intent doing the work, not a message
    /// the classifier was never going to card.
    #[tokio::test]
    async fn just_chatting_withholds_the_card_the_triage_would_have_opened() {
        use crate::ports::tasks::TaskDeliverable;

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let app = router(state);

        // Work by construction — the request frame beats the interrogative, so
        // the triage names a title and the card branch fires.
        let text = "can you build the landing page?";
        assert!(
            matches!(
                crate::company::task_intent::triage_message(text),
                crate::company::task_intent::MessageTriage::Track(_)
            ),
            "fixture must be a message the handler cards, or this proves nothing"
        );

        let chat = |intent: Option<&str>| {
            let body = match intent {
                Some(i) => format!(
                    r#"{{"text":{},"deliverable":"{i}"}}"#,
                    serde_json::json!(text)
                ),
                None => format!(r#"{{"text":{}}}"#, serde_json::json!(text)),
            };
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };

        // `chat`: the operator's statement outranks the classifier's `Track`.
        let r = app.clone().oneshot(chat(Some("chat"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "the message is still answered");
        assert!(
            runtime.tasks().list(&id).await.unwrap().is_empty(),
            "a message sent as chat must open no card, whatever the triage read"
        );

        // The same words, unmarked: the card the run above withheld.
        let r = app.clone().oneshot(chat(None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "an unmarked message is unchanged — this is what the `chat` run withheld"
        );
        assert_eq!(tasks[0].deliverable, TaskDeliverable::Once);

        // …and so are both work words, on the same words again.
        let r = app.clone().oneshot(chat(Some("once"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            runtime.tasks().list(&id).await.unwrap().len(),
            2,
            "`once` is unchanged"
        );

        let r = app.oneshot(chat(Some("workflow"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 3, "`workflow` is unchanged");
        assert!(
            tasks
                .iter()
                .any(|t| t.deliverable == TaskDeliverable::Workflow),
            "and still routes its card to the builder pass: {tasks:?}"
        );
    }

    /// Issue #576: **who** asked decides whether the card self-promotes.
    ///
    /// The promotion buys a planning pass, which is a model call. A person
    /// spending one on their own typo is the cost the issue accepts; an agent
    /// doing it is a loop — a card that plans, whose pass opens further cards,
    /// which promote, which plan, with no human anywhere in it. So the branch is
    /// on the actor, and this pins both sides of it.
    ///
    /// Driven through `run_chat` directly rather than the route, because the
    /// route's job is to *resolve* the actor and this test's job is to pin what
    /// each resolved actor does. Going through HTTP would only ever exercise
    /// whichever principal the test harness happens to authenticate as.
    #[tokio::test]
    async fn only_a_person_gets_a_self_promoting_card() {
        use crate::ports::tasks::{COLUMN_PLANNING, COLUMN_TODO};
        use crate::ports::types::{Actor, ActorKind};

        let ask = "build the landing page";
        let person = Actor {
            kind: ActorKind::User,
            id: "u-1".to_string(),
        };

        // Every actor that is not a person must leave the card where it has
        // always landed. `None` is a machine credential — the platform, or any
        // caller with no session behind it.
        for (label, by, expected) in [
            ("a signed-in user", Some(person.clone()), COLUMN_PLANNING),
            (
                "an operator",
                Some(Actor {
                    kind: ActorKind::Operator,
                    id: "op".to_string(),
                }),
                COLUMN_PLANNING,
            ),
            (
                "an agent",
                Some(Actor {
                    kind: ActorKind::Agent,
                    id: "ceo".to_string(),
                }),
                COLUMN_TODO,
            ),
            (
                "the runtime itself",
                Some(Actor {
                    kind: ActorKind::System,
                    id: "system".to_string(),
                }),
                COLUMN_TODO,
            ),
            ("a machine credential", None, COLUMN_TODO),
        ] {
            let home_dir = home();
            let state = state_with_company(home_dir.path(), "running").await;
            let id = CompanyId::new("acme");
            let runtime = state.registry().get(&id).unwrap();

            let message = ChatMessage {
                mentions: None,
                text: ask.to_string(),
                chat: None,
                parent: None,
                deliverable: None,
                detach: false,
                attachments: Vec::new(),
            };
            let accepted = accept_chat_turn(
                &runtime,
                &id,
                &message,
                by.as_ref(),
                None,
                crate::server::ops::language::DEFAULT_DESK,
            )
            .await
            .expect("the turn is accepted");
            run_chat(runtime.clone(), message, by, &accepted)
                .await
                .expect("the chat cycle runs");

            let tasks = runtime.tasks().list(&id).await.unwrap();
            assert_eq!(tasks.len(), 1, "{label}: one ask opens one card");
            assert_eq!(
                tasks[0].column, expected,
                "{label}: the card must land in `{expected}`"
            );
        }
    }

    /// End-to-end proof of the WS4 wire: with a [`HarnessBrain`] as the runtime's
    /// cognition, `POST /company/chat` returns the **agent's** reply rather than
    /// the echo brain's `"You said: …"`. The mock provider prefixes the routed
    /// message, so `"mock: hi"` proves the operator message reached an openhuman
    /// agent turn through the HTTP handler → `run_cycle` → brain path.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn chat_routes_through_the_harness_brain() {
        use crate::harness::provider::MockProvider;
        use crate::harness::{HarnessBrain, HarnessDeps, HarnessPool};
        use crate::ports::CompanyStore;
        use crate::store::{FsContextStore, FsOps};

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let id = CompanyId::new("acme");
        let manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief Executive\"\n",
        )
        .unwrap();

        let record = CompanyRecord {
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
        };
        FsCompanyStore::new(home.to_path_buf())
            .save(&record)
            .await
            .unwrap();

        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(home.to_path_buf())),
            store: Arc::new(FsCompanyStore::new(home.to_path_buf())),
            meter: Some(Arc::new(FsOps::new(home.to_path_buf()))),
            workspace_root: home.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: home.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: crate::harness::orchestrator::DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record);

        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .with_brain(Arc::new(brain))
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    // Issue #1725: not "hi". A bare pleasantry is answered by
                    // the runtime without a turn, so it would reach no brain at
                    // all — which is the opposite of what this asserts.
                    .body(Body::from(r#"{"text":"ship the landing page"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let text = value["responses"][0]["text"].as_str().unwrap();
        // The mock provider's `mock: ` prefix proves the message went through an
        // openhuman agent turn; the trailing words are the operator message the
        // agent forwarded (the agent prepends a date/time context line).
        // Crucially it is NOT the echo brain's `"You said: …"`.
        assert!(text.starts_with("mock: "), "not an agent reply: {text:?}");
        assert!(
            text.trim_end().ends_with("ship the landing page"),
            "message not forwarded: {text:?}"
        );
        assert_ne!(
            text, "You said: ship the landing page",
            "still routing through the echo brain"
        );
        assert_eq!(value["responses"][0]["channel"], "operator");
    }

    /// A manifest with two agents and one desk (`studio`, led by `ceo`), used by
    /// the desk-membership write tests.
    fn desk_manifest() -> CompanyManifest {
        toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n\
             [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\nmembers = [\"ceo\"]\n",
        )
        .unwrap()
    }

    /// Builds an app state whose sole company carries `manifest`.
    async fn state_with_manifest(home: &std::path::Path, manifest: CompanyManifest) -> AppState {
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        use crate::ports::CompanyStore;
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
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn get_desks(app: &axum::Router, cookie: &str) -> serde_json::Value {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/desks")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Adding an overlay member persists it and surfaces it in `list_desks` as
    /// both an effective member and a removable overlay member.
    #[tokio::test]
    async fn add_desk_member_persists_and_shows_in_list() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let add = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks/studio/members")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"agent_id":"eng"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(add.status(), StatusCode::NO_CONTENT);

        let desks = get_desks(&app, &cookie).await;
        assert_eq!(desks[0]["id"], "studio");
        // Manifest member first, overlay member appended.
        assert_eq!(desks[0]["members"][0], "ceo");
        assert_eq!(desks[0]["members"][1], "eng");
        assert_eq!(desks[0]["overlayMembers"][0], "eng");
    }

    /// Issue #1781 review (Codex P2): an overlay desk whose own id is a
    /// General spelling (`general` or `main`) must not appear in `GET
    /// .../desks` — `POST .../desks` has refused those ids since issue #1743,
    /// so the only way one exists is a company upgraded from before that
    /// guard, and `CompanyRecord::resolve_desk_id` already excludes exactly
    /// this desk from routing. Listing it anyway would let `buildChannels`
    /// (frontend) treat it as the company-wide line and suppress the real
    /// built-in `#general` — showing edit/delete controls and a membership
    /// list that has nothing to do with where a message actually lands.
    ///
    /// Seeded directly on the stored record, not through `POST .../desks`:
    /// that route's own guard means this shape can only be reached by data
    /// that predates it, exactly the grandfathered case this proves.
    #[tokio::test]
    async fn list_desks_hides_an_overlay_desk_shadowing_general() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        let mut record = runtime.store().load(&id).await.unwrap().unwrap();
        record.overlay_desks.push(OverlayDesk {
            id: "general".to_string(),
            name: "General".to_string(),
            description: None,
            members: vec!["ceo".to_string()],
            responder: ResponderMode::Lead,
        });
        record.overlay_desks.push(OverlayDesk {
            id: "main".to_string(),
            name: "Front office".to_string(),
            description: None,
            members: vec!["eng".to_string()],
            responder: ResponderMode::Lead,
        });
        runtime.store().save(&record).await.unwrap();

        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        let desks = get_desks(&app, &cookie).await;
        let ids: Vec<&str> = desks
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap())
            .collect();

        assert!(
            !ids.contains(&"general"),
            "an overlay desk at the reserved `general` id must not be listed: {ids:?}"
        );
        assert!(
            !ids.contains(&"main"),
            "an overlay desk at the reserved `main` id must not be listed: {ids:?}"
        );
        // The manifest desk and a non-shadowing overlay desk are unaffected —
        // this narrows one id, it does not hide desks generally.
        assert!(ids.contains(&"studio"), "unrelated desk dropped: {ids:?}");
    }

    /// Every desk mutation aimed at a bare General spelling — no legacy
    /// overlay row at all — is refused with a reason, under **every** spelling
    /// the host folds into the General conversation (issue #1743; restored PR
    /// #1781 review, CodeRabbit P2).
    ///
    /// This is the `is_general_channel` guard originally added by `da98130c1`
    /// and its own regression test; an unrelated refactor (`3cbdb7a5f`) deleted
    /// the guard, the four call sites, and this test together, and only the
    /// read-side projection filter (`list_desks`/`resolve_desk_id`) was ever
    /// restored (`0c07873db`) — this proves the write side is closed again.
    ///
    /// The point of the assertion is the pair: a `409` **and** the sentence.
    /// Before this guard, each of these was a bare `404`/`CompanyNotFound` —
    /// "there is no such desk" — which is a different and wrong claim.
    /// `#general` is not missing; it is reserved, and the caller needs to be
    /// told which.
    #[tokio::test]
    async fn every_desk_mutation_aimed_at_a_bare_general_spelling_is_refused_with_a_reason() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        for spelling in ["general", "General", "GENERAL", "main", "Main"] {
            let cases: [(&str, String, &str); 4] = [
                ("DELETE", format!("/api/v1/company/desks/{spelling}"), ""),
                (
                    "POST",
                    format!("/api/v1/company/desks/{spelling}/members"),
                    r#"{"agent_id":"eng"}"#,
                ),
                (
                    "DELETE",
                    format!("/api/v1/company/desks/{spelling}/members/ceo"),
                    "",
                ),
                (
                    "PUT",
                    format!("/api/v1/company/desks/{spelling}/order"),
                    r#"{"ordered_member_ids":["ceo"]}"#,
                ),
            ];
            for (method, uri, body) in cases {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(&uri)
                            .header("cookie", &cookie)
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::CONFLICT,
                    "{method} {uri} must be refused, not answered 404"
                );
                let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let text = String::from_utf8_lossy(&bytes);
                assert!(
                    text.contains("company-wide channel"),
                    "{method} {uri} must say why: got {text}"
                );
            }
        }
    }

    /// Sibling to [`list_desks_hides_an_overlay_desk_shadowing_general`]: the
    /// same grandfathered overlay desk at the reserved `general` id — which
    /// that test proves is hidden from `GET .../desks` and unroutable through
    /// [`CompanyRecord::resolve_desk_id`] — must also be unreachable through
    /// every desk *mutation* (issue #1781 review, CodeRabbit P2). Before this
    /// guard was restored, `desk_exists("general")` was `true` for exactly this
    /// desk (it really is in `overlay_desks`), so `add_desk_member`,
    /// `remove_desk_member`, `set_desk_order`, and `delete_desk` — which
    /// checked only `desk_exists` — would staff, reorder, or delete a desk no
    /// read surface exposes at all.
    ///
    /// Seeded directly on the stored record, the same way the read-side sibling
    /// test is: `POST .../desks` has refused this id since issue #1743, so the
    /// only way this shape exists is data that predates that guard.
    #[tokio::test]
    async fn desk_mutations_refuse_a_grandfathered_overlay_desk_shadowing_general() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        let mut record = runtime.store().load(&id).await.unwrap().unwrap();
        record.overlay_desks.push(OverlayDesk {
            id: "general".to_string(),
            name: "General".to_string(),
            description: None,
            members: vec!["ceo".to_string()],
            responder: ResponderMode::Lead,
        });
        runtime.store().save(&record).await.unwrap();

        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let cases: [(&str, &str, &str); 4] = [
            ("DELETE", "/api/v1/company/desks/general", ""),
            (
                "POST",
                "/api/v1/company/desks/general/members",
                r#"{"agent_id":"eng"}"#,
            ),
            ("DELETE", "/api/v1/company/desks/general/members/ceo", ""),
            (
                "PUT",
                "/api/v1/company/desks/general/order",
                r#"{"ordered_member_ids":["ceo"]}"#,
            ),
        ];
        for (method, uri, body) in cases {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("cookie", &cookie)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::CONFLICT,
                "{method} {uri} must be refused even though the desk really \
                 exists in the overlay — desk_exists alone is not enough"
            );
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                text.contains("company-wide channel"),
                "{method} {uri} must say why: got {text}"
            );
        }
    }

    /// `add_desk_member` must serialize its load-modify-save cycle against
    /// `company_write_lock`, exactly like every other console load-modify-save
    /// write (`put_logo`, `set_lifecycle`, `patch_company`) — otherwise it can
    /// silently revert a concurrent rename: `patch_company` is guarded by
    /// `company_write_lock` alone, so a desk write racing in on only the
    /// unrelated `serial` cycle lock can load the pre-rename record and save
    /// the whole thing back after the rename lands (PR #1875 review finding).
    /// Proven the same way `put_logo_serializes_against_the_company_write_lock`
    /// proves it: hold the lock externally, drive the real handler through the
    /// router, and demand it cannot finish while the lock is held.
    #[tokio::test]
    async fn add_desk_member_serializes_against_the_company_write_lock() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        let id = CompanyId::new("acme");

        let lock = company_write_lock(&id);
        let guard = lock.lock().await;

        let app_for_task = app.clone();
        let cookie_for_task = cookie.clone();
        let mut task = tokio::spawn(async move {
            app_for_task
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/company/desks/studio/members")
                        .header("cookie", &cookie_for_task)
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"agent_id":"eng"}"#))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        });

        // The handler must be blocked behind the held lock — give it every
        // chance to (wrongly) race ahead before declaring it stuck.
        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "add_desk_member completed while company_write_lock was held \
             elsewhere — it is not serializing its load-modify-save cycle \
             against concurrent `ops` writers"
        );

        drop(guard);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("add_desk_member never resumed after the lock was released")
            .expect("add_desk_member task panicked");
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    /// `set_desk_order` must serialize against `company_write_lock` too — same
    /// load-modify-save shape and same finding as `add_desk_member`'s own test
    /// above (PR #1875 review finding, round 9: the earlier fix covered five
    /// handlers but this coverage only proved it for one).
    #[tokio::test]
    async fn set_desk_order_serializes_against_the_company_write_lock() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        let id = CompanyId::new("acme");
        seed_overlay_eng(&app, &cookie).await;

        let lock = company_write_lock(&id);
        let guard = lock.lock().await;

        let app_for_task = app.clone();
        let cookie_for_task = cookie.clone();
        let mut task = tokio::spawn(async move {
            put_desk_order(
                &app_for_task,
                &cookie_for_task,
                "studio",
                r#"{"ordered_member_ids":["eng","ceo"]}"#,
            )
            .await
        });

        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "set_desk_order completed while company_write_lock was held \
             elsewhere — it is not serializing its load-modify-save cycle \
             against concurrent `ops` writers"
        );

        drop(guard);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("set_desk_order never resumed after the lock was released")
            .expect("set_desk_order task panicked");
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    /// `remove_desk_member` must serialize against `company_write_lock` too
    /// (PR #1875 review finding, round 9 — see
    /// `set_desk_order_serializes_against_the_company_write_lock`'s own doc).
    #[tokio::test]
    async fn remove_desk_member_serializes_against_the_company_write_lock() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        let id = CompanyId::new("acme");
        seed_overlay_eng(&app, &cookie).await;

        let lock = company_write_lock(&id);
        let guard = lock.lock().await;

        let app_for_task = app.clone();
        let cookie_for_task = cookie.clone();
        let mut task = tokio::spawn(async move {
            app_for_task
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri("/api/v1/company/desks/studio/members/eng")
                        .header("cookie", &cookie_for_task)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        });

        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "remove_desk_member completed while company_write_lock was held \
             elsewhere — it is not serializing its load-modify-save cycle \
             against concurrent `ops` writers"
        );

        drop(guard);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("remove_desk_member never resumed after the lock was released")
            .expect("remove_desk_member task panicked");
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    /// `create_desk` must serialize against `company_write_lock` too (PR #1875
    /// review finding, round 9 — see
    /// `set_desk_order_serializes_against_the_company_write_lock`'s own doc).
    #[tokio::test]
    async fn create_desk_serializes_against_the_company_write_lock() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        let id = CompanyId::new("acme");

        let lock = company_write_lock(&id);
        let guard = lock.lock().await;

        let app_for_task = app.clone();
        let cookie_for_task = cookie.clone();
        let mut task = tokio::spawn(async move {
            app_for_task
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/company/desks")
                        .header("cookie", &cookie_for_task)
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"name":"Growth","members":["eng"]}"#))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        });

        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "create_desk completed while company_write_lock was held \
             elsewhere — it is not serializing its load-modify-save cycle \
             against concurrent `ops` writers"
        );

        drop(guard);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("create_desk never resumed after the lock was released")
            .expect("create_desk task panicked");
        assert_eq!(status, StatusCode::CREATED);
    }

    /// `delete_desk` must serialize against `company_write_lock` too (PR #1875
    /// review finding, round 9 — see
    /// `set_desk_order_serializes_against_the_company_write_lock`'s own doc).
    #[tokio::test]
    async fn delete_desk_serializes_against_the_company_write_lock() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        let id = CompanyId::new("acme");

        // Create the overlay desk to delete before taking the lock — this
        // test proves serialization on the delete path, not the create path.
        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"Growth","members":["eng"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);

        let lock = company_write_lock(&id);
        let guard = lock.lock().await;

        let app_for_task = app.clone();
        let cookie_for_task = cookie.clone();
        let mut task = tokio::spawn(async move {
            app_for_task
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri("/api/v1/company/desks/growth")
                        .header("cookie", &cookie_for_task)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        });

        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "delete_desk completed while company_write_lock was held \
             elsewhere — it is not serializing its load-modify-save cycle \
             against concurrent `ops` writers"
        );

        drop(guard);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("delete_desk never resumed after the lock was released")
            .expect("delete_desk task panicked");
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    /// Removing an overlay member drops it from the merged view; a manifest
    /// member cannot be removed (409), and an unknown overlay member is a 404.
    #[tokio::test]
    async fn remove_desk_member_drops_overlay_and_guards_manifest() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        // Seed an overlay member.
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks/studio/members")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"agent_id":"eng"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Removing a manifest member is a 409.
        let manifest_remove = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/desks/studio/members/ceo")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(manifest_remove.status(), StatusCode::CONFLICT);

        // Removing the overlay member succeeds and drops it from the list.
        let remove = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/desks/studio/members/eng")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(remove.status(), StatusCode::NO_CONTENT);

        let desks = get_desks(&app, &cookie).await;
        assert_eq!(desks[0]["members"].as_array().unwrap().len(), 1);
        assert!(desks[0].get("overlayMembers").is_none());

        // Removing it again is a 404 (no such overlay member).
        let gone = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/desks/studio/members/eng")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    }

    /// Creates an overlay desk through the same route `create_desk` serves and
    /// returns its derived id, so the desk under test exists only in the overlay
    /// — nothing about it is declared in the manifest.
    async fn seed_overlay_desk(app: &axum::Router, cookie: &str, body: &str) -> String {
        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks")
                    .header("cookie", cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let bytes = to_bytes(created.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["id"].as_str().unwrap().to_string()
    }

    async fn post_desk_member(
        app: &axum::Router,
        cookie: &str,
        desk: &str,
        body: &str,
    ) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/company/desks/{desk}/members"))
                    .header("cookie", cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn delete_desk_member(
        app: &axum::Router,
        cookie: &str,
        desk: &str,
        agent: &str,
    ) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/company/desks/{desk}/members/{agent}"))
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// Reads the `error` string out of an api.md error envelope.
    async fn error_message(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["error"].as_str().unwrap().to_string()
    }

    /// Returns the effective member list of `desk` from `list_desks`.
    async fn desk_members(app: &axum::Router, cookie: &str, desk: &str) -> Vec<String> {
        let desks = get_desks(app, cookie).await;
        desks
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == desk)
            .unwrap_or_else(|| panic!("desk {desk} present in list"))["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect()
    }

    /// A desk that exists only in the operator overlay can be staffed and
    /// unstaffed like a manifest desk. Both membership handlers used to test the
    /// manifest alone, so a console-created desk could be reordered and deleted
    /// but never gain or lose a member (#833). Every other desk test seeds its
    /// desk from the manifest, so only an overlay-created desk exercises this.
    #[tokio::test]
    async fn desk_member_writes_reach_an_overlay_created_desk() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let desk =
            seed_overlay_desk(&app, &cookie, r#"{"name":"Growth desk","members":["ceo"]}"#).await;
        assert_eq!(desk, "growth_desk");
        assert_eq!(desk_members(&app, &cookie, &desk).await, ["ceo"]);

        let added = post_desk_member(&app, &cookie, &desk, r#"{"agent_id":"eng"}"#).await;
        assert_eq!(added.status(), StatusCode::NO_CONTENT);
        assert_eq!(desk_members(&app, &cookie, &desk).await, ["ceo", "eng"]);

        let removed = delete_desk_member(&app, &cookie, &desk, "eng").await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        assert_eq!(desk_members(&app, &cookie, &desk).await, ["ceo"]);
    }

    /// An unknown desk id is refused as a missing desk, not a missing company.
    /// The refusal used to be raised as `CompanyNotFound("desk ghost")`, which
    /// rendered as `company not found: desk ghost` — the wrong resource, and the
    /// desk id stuffed into a company id slot (#833). The status stays `404`
    /// because both variants map there.
    #[tokio::test]
    async fn unknown_desk_member_writes_refuse_as_a_missing_desk() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let added = post_desk_member(&app, &cookie, "ghost", r#"{"agent_id":"eng"}"#).await;
        assert_eq!(added.status(), StatusCode::NOT_FOUND);
        let message = error_message(added).await;
        assert!(
            !message.contains("company not found"),
            "add refusal blames the company: {message:?}"
        );
        assert!(message.contains("ghost"), "add refusal drops the desk id");

        let removed = delete_desk_member(&app, &cookie, "ghost", "eng").await;
        assert_eq!(removed.status(), StatusCode::NOT_FOUND);
        let message = error_message(removed).await;
        assert!(
            !message.contains("company not found"),
            "remove refusal blames the company: {message:?}"
        );
        assert!(
            message.contains("ghost"),
            "remove refusal drops the desk id"
        );
    }

    /// Creating a desk persists it as an overlay and surfaces it in `list_desks`
    /// alongside the manifest desks, flagged `overlayCreated` with its lead
    /// first. The manifest is never rewritten.
    #[tokio::test]
    async fn create_desk_persists_and_appears_in_list() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"Growth desk","description":"Acquisition.","members":["eng","ceo"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let bytes = to_bytes(created.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Id derived from the name; the first member is the lead.
        assert_eq!(body["id"], "growth_desk");
        assert_eq!(body["name"], "Growth desk");
        assert_eq!(body["overlayCreated"], true);
        assert_eq!(body["members"][0], "eng");
        assert_eq!(body["members"][1], "ceo");

        // The list now carries the manifest desk and the created overlay desk.
        // The Operator feed is its own surface (issue #1757 rework) — it is
        // fetched through `GET {scope}/operator-channel`, not injected here.
        let desks = get_desks(&app, &cookie).await;
        let arr = desks.as_array().unwrap();
        assert_eq!(arr.len(), 2, "{arr:?}");
        assert_eq!(arr[0]["id"], "studio"); // manifest desk first
        assert_eq!(arr[1]["id"], "growth_desk");
        assert_eq!(arr[1]["overlayCreated"], true);
    }

    /// Issue #1835, both wire directions. A create that never mentions
    /// `responder` — every existing caller, and the org chart today — answers
    /// and lists with **no** `responder` key at all, so old consoles see the
    /// pre-#1835 shape byte-for-byte. A create with `responder: "auto"`
    /// answers and lists `"auto"`, and the mode survives the store round-trip
    /// rather than collapsing back to a lead desk.
    #[tokio::test]
    async fn create_desk_carries_the_responder_mode_and_omits_the_default() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let post = |body: &'static str| {
            let app = app.clone();
            let cookie = cookie.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/v1/company/desks")
                            .header("cookie", &cookie)
                            .header("content-type", "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::CREATED);
                let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
            }
        };

        let lead = post(r#"{"name":"Growth desk","members":["eng"]}"#).await;
        assert!(
            lead.get("responder").is_none(),
            "a mode never stated must not appear on the wire: {lead}"
        );
        let auto =
            post(r#"{"name":"Launch week","members":["eng","ceo"],"responder":"auto"}"#).await;
        assert_eq!(auto["responder"], "auto", "{auto}");

        // The list re-reads the store, so this is the round-trip half: the
        // manifest desk and the defaulted create stay keyless, the channel
        // keeps its mode.
        let desks = get_desks(&app, &cookie).await;
        let arr = desks.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert!(arr[0].get("responder").is_none(), "manifest desk: {desks}");
        assert!(
            arr[1].get("responder").is_none(),
            "defaulted create: {desks}"
        );
        assert_eq!(arr[2]["responder"], "auto", "{desks}");
    }

    /// Issue #1835, codex review: an `auto` channel cannot be created empty —
    /// the selector would have no candidates and the first-member fallback no
    /// first member, so its unmentioned messages would silently fall to the
    /// orchestrator, contradicting the channel's own model. A **lead** desk
    /// keeps its right to start empty and be staffed from the org chart.
    /// Revert the guard in `create_desk` and the first assertion answers 201.
    #[tokio::test]
    async fn an_auto_channel_cannot_be_created_empty_but_a_lead_desk_still_can() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let post = |body: &'static str| {
            let app = app.clone();
            let cookie = cookie.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/company/desks")
                        .header("cookie", &cookie)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        let refused = post(r#"{"name":"Launch week","responder":"auto"}"#).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(refused.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).to_string();
        assert!(
            body.contains("at least one member"),
            "the refusal names the reason, not a generic 400: {body}"
        );

        let empty_lead = post(r#"{"name":"Someday desk"}"#).await;
        assert_eq!(
            empty_lead.status(),
            StatusCode::CREATED,
            "an empty lead desk is still legal — it gains members from the org chart"
        );
    }

    /// Create-desk validation: an empty name is 400, an id colliding with a
    /// manifest desk is 409, an unknown member is 400, and — issue #1757 — an
    /// id (explicit or name-derived) colliding with the reserved `operator`
    /// system channel is 409 even though it is not a manifest or overlay desk
    /// `desk_exists` would otherwise catch.
    #[tokio::test]
    async fn create_desk_validates_name_id_and_members() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let cases = [
            (r#"{"name":"   "}"#, StatusCode::BAD_REQUEST),
            (r#"{"name":"Studio","id":"studio"}"#, StatusCode::CONFLICT),
            (
                r#"{"name":"Ghost desk","members":["ghost"]}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"name":"Operator","id":"operator"}"#,
                StatusCode::CONFLICT,
            ),
            (r#"{"name":"operator"}"#, StatusCode::CONFLICT),
            // PR #1781 review (CodeRabbit P2 follow-up to `316bc9229`): the id
            // guard alone lets a display-name collision through — `{"id":
            // "ops", "name": "Operator"}` never touches the reserved id, but
            // `resolve_desk_id` would still fold a `?desk=Operator` selector
            // onto this desk exactly as it would onto one literally named
            // `operator`. Same shape for the collision-fallback display name.
            (r#"{"name":"Operator","id":"ops"}"#, StatusCode::CONFLICT),
            (
                r#"{"name":"operator-feed","id":"ops2"}"#,
                StatusCode::CONFLICT,
            ),
            // Issue #1743 / PR #1781 review: a desk claiming a General
            // spelling — by id or by display name — would shadow the
            // built-in `#general` channel exactly as an `operator`-id desk
            // shadows the Operator feed.
            (r#"{"name":"Ops","id":"general"}"#, StatusCode::CONFLICT),
            (r#"{"name":"Ops","id":"main"}"#, StatusCode::CONFLICT),
            (r#"{"name":"General"}"#, StatusCode::CONFLICT),
            (r#"{"name":"Main"}"#, StatusCode::CONFLICT),
        ];
        for (body, want) in cases {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/company/desks")
                        .header("cookie", &cookie)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), want, "body {body}");
        }
    }

    /// Deleting an operator-created desk drops it (and any of its overlay
    /// members); a manifest desk cannot be deleted (409); an unknown id is 404.
    #[tokio::test]
    async fn delete_desk_removes_overlay_and_guards_manifest() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        // Create an overlay desk to delete.
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"Growth","members":["eng"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // A manifest desk cannot be deleted.
        let manifest_delete = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/desks/studio")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(manifest_delete.status(), StatusCode::CONFLICT);

        // The overlay desk deletes and drops out of the list.
        let delete = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/desks/growth")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete.status(), StatusCode::NO_CONTENT);

        let desks = get_desks(&app, &cookie).await;
        // Only the manifest desk remains — the Operator feed is its own
        // surface now (issue #1757 rework), not injected into this list.
        let arr = desks.as_array().unwrap();
        assert_eq!(arr.len(), 1, "{arr:?}");
        assert_eq!(arr[0]["id"], "studio");

        // Deleting it again is a 404.
        let gone = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/desks/growth")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    }

    /// Issue #1781 review (Codex P2): deleting a legacy overlay desk that was
    /// holding `operator_feed_channel()` on the fallback address must not let
    /// it revert to `OPERATOR_CHANNEL`.
    ///
    /// `desk_exists`/`resolve_desk_id` are live checks — with no tombstone,
    /// removing the colliding desk makes them stop matching, so the divert
    /// would silently flip back the moment `delete_desk` succeeds. Seeded
    /// directly on the stored record rather than through `POST .../desks`
    /// (as `list_desks_hides_an_overlay_desk_shadowing_general` does for its
    /// own General case): `create_desk`'s own guard has refused the id and
    /// name `operator` since `316bc9229`, so this shape can only be reached
    /// by an overlay desk that predates it — exactly what this proves stays
    /// safe to delete.
    #[tokio::test]
    async fn delete_desk_keeps_the_operator_feed_diverted_after_the_collision_is_gone() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        let mut record = runtime.store().load(&id).await.unwrap().unwrap();
        record.overlay_desks.push(OverlayDesk {
            id: "operator".to_string(),
            name: "Legacy Ops".to_string(),
            description: None,
            members: vec![],
            responder: ResponderMode::Lead,
        });
        runtime.store().save(&record).await.unwrap();

        let reloaded = runtime.store().load(&id).await.unwrap().unwrap();
        assert_eq!(
            reloaded.operator_feed_channel(),
            crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK,
            "fixture must start in the collision state this test exercises"
        );

        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        let delete = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/desks/operator")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete.status(), StatusCode::NO_CONTENT);

        let after = runtime.store().load(&id).await.unwrap().unwrap();
        assert!(
            !after.desk_exists(crate::runtime::channel::OPERATOR_CHANNEL),
            "the colliding desk must actually be gone, or this is not \
             exercising the live-check-flips-back failure mode at all"
        );
        assert_eq!(
            after.operator_feed_channel(),
            crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK,
            "the feed address must stay on the fallback once the desk that \
             caused the collision is deleted — flipping back to \
             OPERATOR_CHANNEL would orphan every report already journaled \
             under the fallback and let the deleted desk's own historical \
             transcript (chat_id == \"operator\") resurface as system-feed \
             content"
        );
    }

    /// Add-member validation: an unknown desk is 404, an unknown teammate is
    /// 400, and a teammate already on the desk is 409.
    #[tokio::test]
    async fn add_desk_member_validates_desk_agent_and_duplicates() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let cases = [
            (
                "/api/v1/company/desks/ghost/members",
                r#"{"agent_id":"eng"}"#,
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/v1/company/desks/studio/members",
                r#"{"agent_id":"ghost"}"#,
                StatusCode::BAD_REQUEST,
            ),
            // `ceo` is already a manifest member of `studio`.
            (
                "/api/v1/company/desks/studio/members",
                r#"{"agent_id":"ceo"}"#,
                StatusCode::CONFLICT,
            ),
        ];
        for (uri, body, want) in cases {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("cookie", &cookie)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), want, "{uri} {body}");
        }
    }

    /// Seeds `eng` as an overlay member of `studio` so a desk has two members to
    /// reorder.
    async fn seed_overlay_eng(app: &axum::Router, cookie: &str) {
        let add = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks/studio/members")
                    .header("cookie", cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"agent_id":"eng"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(add.status(), StatusCode::NO_CONTENT);
    }

    async fn put_desk_order(
        app: &axum::Router,
        cookie: &str,
        desk: &str,
        body: &str,
    ) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/company/desks/{desk}/order"))
                    .header("cookie", cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    /// A `PUT .../order` reorders the desk; the change surfaces in `list_desks`
    /// as the new `members` order (the hierarchy), and an empty body resets it.
    #[tokio::test]
    async fn set_desk_order_reorders_and_resets() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        seed_overlay_eng(&app, &cookie).await;

        // Base order is manifest-first: ceo, then the overlay eng.
        let desks = get_desks(&app, &cookie).await;
        assert_eq!(desks[0]["members"][0], "ceo");
        assert_eq!(desks[0]["members"][1], "eng");

        // Promote the overlay member to the lead slot.
        let status = put_desk_order(
            &app,
            &cookie,
            "studio",
            r#"{"ordered_member_ids":["eng","ceo"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let desks = get_desks(&app, &cookie).await;
        assert_eq!(desks[0]["members"][0], "eng");
        assert_eq!(desks[0]["members"][1], "ceo");

        // An empty body clears the override, restoring the blueprint order.
        let status = put_desk_order(&app, &cookie, "studio", r#"{"ordered_member_ids":[]}"#).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let desks = get_desks(&app, &cookie).await;
        assert_eq!(desks[0]["members"][0], "ceo");
        assert_eq!(desks[0]["members"][1], "eng");
    }

    /// An operator-created (overlay) desk can be reordered too — the set-order
    /// handler validates existence with `desk_exists`, which covers overlay desks,
    /// not just manifest group chats. A manifest-only check used to 404 here (#133).
    #[tokio::test]
    async fn set_desk_order_reorders_an_overlay_created_desk() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        // Create an overlay desk with two members (lead is `ceo` by declaration).
        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"Growth desk","members":["ceo","eng"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);

        // Reordering the overlay desk succeeds (not 404) and promotes `eng`.
        let status = put_desk_order(
            &app,
            &cookie,
            "growth_desk",
            r#"{"ordered_member_ids":["eng","ceo"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // The new hierarchy surfaces in the list for the overlay desk.
        let desks = get_desks(&app, &cookie).await;
        let growth = desks
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == "growth_desk")
            .expect("overlay desk present");
        assert_eq!(growth["members"][0], "eng");
        assert_eq!(growth["members"][1], "ceo");
    }

    /// Set-order validation: an unknown desk is 404, an unknown member id is 400,
    /// and a duplicate id is 400.
    #[tokio::test]
    async fn set_desk_order_validates_desk_members_and_duplicates() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        seed_overlay_eng(&app, &cookie).await;

        // Unknown desk → 404.
        assert_eq!(
            put_desk_order(&app, &cookie, "ghost", r#"{"ordered_member_ids":["ceo"]}"#).await,
            StatusCode::NOT_FOUND
        );
        // A non-member id → 400.
        assert_eq!(
            put_desk_order(
                &app,
                &cookie,
                "studio",
                r#"{"ordered_member_ids":["ceo","ghost"]}"#
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        // Duplicate id → 400.
        assert_eq!(
            put_desk_order(
                &app,
                &cookie,
                "studio",
                r#"{"ordered_member_ids":["ceo","ceo"]}"#
            )
            .await,
            StatusCode::BAD_REQUEST
        );
    }

    /// Removing an overlay member prunes it from the desk's order overlay, so the
    /// remaining members keep the operator's relative order without a stale id.
    #[tokio::test]
    async fn remove_desk_member_prunes_the_order_entry() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");
        seed_overlay_eng(&app, &cookie).await;

        // Reorder to [eng, ceo], then remove eng.
        assert_eq!(
            put_desk_order(
                &app,
                &cookie,
                "studio",
                r#"{"ordered_member_ids":["eng","ceo"]}"#
            )
            .await,
            StatusCode::NO_CONTENT
        );
        let remove = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/desks/studio/members/eng")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(remove.status(), StatusCode::NO_CONTENT);

        // Only the manifest member remains; the order entry is gone (no stale
        // eng lingering), so ceo is the lead.
        let desks = get_desks(&app, &cookie).await;
        assert_eq!(desks[0]["members"].as_array().unwrap().len(), 1);
        assert_eq!(desks[0]["members"][0], "ceo");
    }

    #[tokio::test]
    async fn desks_route_returns_the_company_desks() {
        // The default test manifest defines no group chats, so the route
        // answers 200 with an empty list — the console falls back to its
        // static default threads. The Operator feed is a separate surface
        // (issue #1757 rework), fetched through `GET
        // {scope}/operator-channel`, and no longer folded into this list.
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/desks")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let desks = value.as_array().unwrap();
        assert!(desks.is_empty(), "{desks:?}");
    }

    async fn get_operator_channel(app: &axum::Router, cookie: &str) -> serde_json::Value {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/operator-channel")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Issue #1757 rework: `GET {scope}/operator-channel` returns the
    /// dedicated feed's identity — never folded into `list_desks` any more —
    /// and `list_desks` carries zero operator logic: the real desks are all
    /// it returns.
    #[tokio::test]
    async fn operator_channel_route_returns_the_feed_identity_and_is_absent_from_desks() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let channel = get_operator_channel(&app, &cookie).await;
        assert_eq!(channel["id"], "operator");
        assert_eq!(channel["name"], "Operator");
        assert!(
            channel["description"]
                .as_str()
                .unwrap()
                .contains("what happened"),
            "{channel}"
        );

        let desks = get_desks(&app, &cookie).await;
        assert!(
            desks
                .as_array()
                .unwrap()
                .iter()
                .all(|d| d["id"] != "operator"),
            "list_desks must carry zero operator logic: {desks:?}"
        );
    }

    /// Issue #1757 rework: the always-present Operator feed is its own
    /// surface — `GET {scope}/operator-channel` names it, `list_desks` never
    /// does — and posting to it is still refused (it is a read-only report
    /// feed).
    #[tokio::test]
    async fn the_operator_channel_is_a_separate_surface_and_stays_read_only() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let desks = get_desks(&app, &cookie).await;
        let desks = desks.as_array().unwrap();
        let ids: Vec<&str> = desks.iter().map(|d| d["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["studio"], "list_desks carries only real desks");

        let channel = get_operator_channel(&app, &cookie).await;
        assert_eq!(channel["id"], "operator");
        assert_eq!(channel["name"], "Operator");

        // A send addressed to it is refused (read-only), never journaled.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi","chat":"operator"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.status().is_client_error(),
            "posting to the operator channel must be refused, got {}",
            response.status()
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).to_lowercase();
        assert!(body.contains("read-only"), "{body}");
    }

    /// Issue #1781 review (CodeRabbit): `CompanyRuntime::ensure_desk_writable`
    /// re-loads the record on every operator-channel send (to catch a
    /// grandfathered desk/teammate colliding with the reserved id) and
    /// propagates a real `store().load` failure with `?` rather than folding
    /// it into "no real recipient". Collapsing it would misreport a store
    /// outage as the ordinary read-only refusal — same 4xx, same message,
    /// same "read-only" wording an operator would wrongly believe.
    ///
    /// Corrupting `company.toml` on disk after the app is built (rather than
    /// mocking `CompanyStore`) exercises the real `FsCompanyStore::load`
    /// error path — `Err(OpenCompanyError::Store("invalid company.toml: …"))`
    /// — which has no `Store` arm in `ApiError::status` and therefore falls
    /// to the catch-all `INTERNAL_SERVER_ERROR`. A collapsed-to-`false` read
    /// would instead surface as `InvalidRequest` (400) with the read-only
    /// wording, so the status code and body together distinguish the two.
    #[tokio::test]
    async fn a_failing_store_load_is_not_collapsed_into_the_read_only_refusal() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        // Corrupt the on-disk manifest so the next `store().load()` — the one
        // `ensure_desk_writable` runs fresh on every send — fails instead of
        // returning `Some(record)`.
        let toml_path = crate::store::Bundle::new(&home, &CompanyId::new("acme")).company_toml();
        tokio::fs::write(&toml_path, b"not valid toml [[[")
            .await
            .expect("corrupt company.toml");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi","chat":"operator"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a store load failure must propagate as itself, not the read-only 4xx"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).to_lowercase();
        assert!(
            !body.contains("read-only"),
            "a store outage must not be misreported as the ordinary read-only refusal: {body}"
        );
    }

    /// CodeRabbit review (PR #1781, P2): `operator_channel` used to fold a
    /// `store().load()` failure into "no record" via `.ok().flatten()`, and
    /// answer the default `operator` id anyway. For an upgraded company whose
    /// grandfathered `operator` teammate requires the `operator-feed`
    /// collision address, that silently mislabels the teammate's `operator`
    /// transcript as the system feed while a transient outage lasts — and the
    /// console would show it as healthy the whole time. This proves the fix:
    /// a real load failure now propagates as an error instead of defaulting.
    ///
    /// Corrupts `company.toml` on disk after the app is built (rather than
    /// mocking `CompanyStore`) to exercise the real `FsCompanyStore::load`
    /// error path — same technique as
    /// `a_failing_store_load_is_not_collapsed_into_the_read_only_refusal`
    /// above.
    #[tokio::test]
    async fn operator_channel_propagates_a_store_load_failure_instead_of_defaulting() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        // Baseline: before any corruption, the route answers the default id.
        let channel = get_operator_channel(&app, &cookie).await;
        assert_eq!(channel["id"], "operator");

        // Corrupt the on-disk manifest so the next `store().load()` fails
        // instead of returning `Some(record)` or `None`.
        let toml_path = crate::store::Bundle::new(&home, &CompanyId::new("acme")).company_toml();
        tokio::fs::write(&toml_path, b"not valid toml [[[")
            .await
            .expect("corrupt company.toml");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/operator-channel")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a store load failure must propagate as itself, not the default operator id"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_ne!(
            body["id"], "operator",
            "a store outage must not be silently answered as the healthy default channel: {body}"
        );
    }

    /// Issue #1757 migration: `operator` was not a reserved id before this
    /// issue, and a stored manifest is never re-validated on load
    /// (`CompanyManifest::from_stored_toml` skips validation on purpose, so
    /// tightening a rule never strands an already-running company) — so a
    /// company provisioned earlier can already have a real `[[group_chat]]`
    /// using that id. Built directly with `toml::from_str` (bypassing
    /// `into_validated`, the same way a stored manifest reaches
    /// `CompanyRuntime` without going through it) to stand in for exactly
    /// that: data that predates the guard. Without the carve-outs in
    /// `list_desks` and `chat_and_emit`, this desk would be shadowed by a
    /// synthetic, read-only duplicate under the same id the moment this
    /// feature shipped, and every send to it would be refused. This proves
    /// it is grandfathered instead: listed once, not flagged `system`, and
    /// still writable.
    #[tokio::test]
    async fn a_manifest_desk_predating_the_reserved_operator_id_stays_writable() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let legacy_manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[group_chat]]\nid = \"operator\"\nname = \"Ops Room\"\nmembers = [\"ceo\"]\n",
        )
        .unwrap();
        let state = state_with_manifest(&home, legacy_manifest).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let desks = get_desks(&app, &cookie).await;
        let desks = desks.as_array().unwrap();
        assert_eq!(desks.len(), 1, "no duplicate synthetic entry: {desks:?}");
        assert_eq!(desks[0]["id"], "operator");
        assert_eq!(
            desks[0]["name"], "Ops Room",
            "the real desk's own name, not the synthetic channel's: {desks:?}"
        );
        assert!(
            desks[0].get("system").is_none(),
            "grandfathered desk is a real desk (system defaults false and is \
             omitted), not the system channel: {desks:?}"
        );

        // A send addressed to it must go through — this is the pre-existing
        // desk's own line, not the (absent) synthetic system channel.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"text":"ship the landing page","chat":"operator"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "a pre-existing desk that already owns the `operator` id must stay \
             writable, got {}",
            response.status()
        );
    }

    /// The name-collision sibling of the id-collision test above (issue #1781
    /// review, Codex P1 follow-up): a manifest desk grandfathered onto the
    /// **display name** `Operator` (`{ id = "legacy_ops", name = "Operator" }`)
    /// rather than the literal id. `resolve_desk_id` — what every *read*
    /// already resolves a `?desk=` selector through — matches this desk by
    /// name just as thoroughly as the id-collision desk above is matched by
    /// id, but `ensure_desk_writable` used to check the *raw* selector string
    /// against `OPERATOR_CHANNEL` before any such resolution ran, so a send
    /// addressed to the desk's own supported alias (`chat: "Operator"`,
    /// case-insensitive) was refused as the read-only system feed — reachable
    /// by name for reads, refused by name for writes, the exact mismatch
    /// `create_desk`'s reservation comment (above) warns a desk can never be
    /// addressed consistently under. A send addressed to the desk's real id
    /// (`legacy_ops`) already sailed through either way, which this also
    /// covers as the negative control.
    #[tokio::test]
    async fn a_manifest_desk_grandfathered_onto_the_operator_name_stays_writable() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let legacy_manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[group_chat]]\nid = \"legacy_ops\"\nname = \"Operator\"\nmembers = [\"ceo\"]\n",
        )
        .unwrap();
        let state = state_with_manifest(&home, legacy_manifest).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        // The desk's own real id still works — this was never broken.
        let by_id = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"by id","chat":"legacy_ops"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            by_id.status().is_success(),
            "a send addressed to the grandfathered desk's real id must stay writable, got {}",
            by_id.status()
        );

        // The desk's supported display-name alias must now work too.
        let by_name = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"by name","chat":"Operator"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            by_name.status().is_success(),
            "a send addressed to the grandfathered desk's own case-insensitive \
             `Operator` alias must resolve to the real desk, not the read-only \
             system feed, got {}",
            by_name.status()
        );
    }

    /// The fallback-address sibling of the test above (issue #1781 review,
    /// Codex P2 follow-up): a manifest desk grandfathered onto the display
    /// name `operator-feed` — `OPERATOR_CHANNEL_COLLISION_FALLBACK` itself —
    /// rather than `Operator`. No desk or teammate here claims the *primary*
    /// `operator` id or name, so `operator_feed_channel()` stays on the
    /// literal address and never diverts; the fallback is purely this desk's
    /// own pre-#1757 display name. `ensure_desk_writable` used to refuse the
    /// fallback constant unconditionally, without resolving it through
    /// `resolve_desk_id` first the way the primary branch does — so a send
    /// addressed to this desk's own supported case-insensitive alias
    /// (`chat: "operator-feed"`) was refused as if it named the synthetic
    /// read-only system desk, even though nothing here is actually diverted.
    /// A send to the desk's real id (`ops`) already sailed through either
    /// way, which this also covers as the negative control.
    #[tokio::test]
    async fn a_manifest_desk_grandfathered_onto_the_fallback_name_stays_writable() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let legacy_manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[group_chat]]\nid = \"ops\"\nname = \"operator-feed\"\nmembers = [\"ceo\"]\n",
        )
        .unwrap();
        let state = state_with_manifest(&home, legacy_manifest).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let record = runtime.store().load(&id).await.unwrap().unwrap();
        assert_eq!(
            record.operator_feed_channel(),
            crate::runtime::channel::OPERATOR_CHANNEL,
            "fixture must NOT be in the diverted state — this proves the \
             fallback name is refused even with no primary collision at all, \
             which the diverted case above does not exercise"
        );
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        // The desk's own real id still works — this was never broken.
        let by_id = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"by id","chat":"ops"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            by_id.status().is_success(),
            "a send addressed to the grandfathered desk's real id must stay writable, got {}",
            by_id.status()
        );

        // The desk's supported display-name alias must now work too.
        let by_name = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"by name","chat":"operator-feed"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            by_name.status().is_success(),
            "a send addressed to the grandfathered desk's own case-insensitive \
             `operator-feed` alias must resolve to the real desk, not the \
             read-only system feed, got {}",
            by_name.status()
        );
    }

    /// Issue #1757 migration, the other namespace: a **teammate**, not a desk,
    /// already named `operator`. `ChatView` addresses a DM by the teammate's
    /// bare id (issue #364), so a message meant for this person also arrives
    /// here as `chat == "operator"` — the same shape as a send meant for the
    /// system feed. `desk_exists` alone cannot tell them apart: it only walks
    /// `group_chats` and `overlay_desks`, never the roster, so a company that
    /// named a manifest agent "Operator" before this feature shipped would
    /// find that teammate's DM permanently refused, with the console giving no
    /// way to rename or migrate out of the collision (`RESERVED_AGENT_IDS` and
    /// `mint_agent_id` only stop a *future* mint). `is_roster_agent` closes the
    /// same gap `desk_exists` closes for desks.
    #[tokio::test]
    async fn a_manifest_agent_predating_the_reserved_operator_id_stays_dm_able() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let legacy_manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"operator\"\nrole = \"Chief of Staff\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n",
        )
        .unwrap();
        let state = state_with_manifest(&home, legacy_manifest).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        // A DM addressed to the grandfathered teammate — by its bare id, the
        // same address `ChatView` sends — must go through rather than be
        // refused as a send to the read-only system channel.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"text":"status update please","chat":"operator"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "a pre-existing teammate that already owns the `operator` id must \
             stay DM-able, got {}",
            response.status()
        );
    }

    /// Issue #1757 rework, the read side of the grandfather case the test
    /// above covers on the write side: a company whose roster names a
    /// teammate `operator` (no desk of the same id) must have `GET
    /// {scope}/operator-channel` answer at the disjoint collision-fallback
    /// id, not the literal `operator` one — a direct post to the visible
    /// read-only feed and the teammate's own DM must stay distinguishable
    /// (`chat_id == "operator"` for the DM, the fallback id for the feed) —
    /// and that fallback id must itself stay refused as read-only.
    #[tokio::test]
    async fn the_operator_channel_diverts_off_a_grandfathered_teammates_operator_line() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let legacy_manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"operator\"\nrole = \"Chief of Staff\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n",
        )
        .unwrap();
        let state = state_with_manifest(&home, legacy_manifest).await;
        let app = router(state.clone());
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let channel = get_operator_channel(&app, &cookie).await;
        assert_eq!(
            channel["id"],
            crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK,
            "the feed must not claim the literal `operator` id once a \
             teammate already holds it: {channel:?}"
        );

        // list_desks carries no operator logic at all, so it is untouched by
        // this collision either way — nothing to assert there but its
        // absence of the teammate, which the DM test above already covers.

        // The disjoint fallback id is unmintable and system-only: a direct post
        // to it must stay refused exactly like the literal `operator` id is,
        // even though nothing minted it as a desk.
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"text":"hello","chat":"{}"}}"#,
                        crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "the disjoint system-feed address must stay read-only"
        );
    }

    /// PR #1781 review (CodeRabbit): the same divert as the test above, for
    /// the *other* grandfather shape — a real **desk** already owning
    /// `operator` (see `a_manifest_desk_predating_the_reserved_operator_id_stays_writable`
    /// for the write side of this same fixture). Left undiverted, `GET
    /// {scope}/operator-channel` and `GET {scope}/desks` would answer the
    /// same id for two different things: the console appends the pinned
    /// Operator row *after* the desk section (`operatorSection`,
    /// `frontend/src/views/ChatView.tsx`), so `findChannel` — first-section-match
    /// — would resolve the pinned row to the desk, and every workflow report
    /// would journal onto the desk's own transcript instead of a
    /// distinguishable feed.
    #[tokio::test]
    async fn the_operator_channel_diverts_off_a_grandfathered_desks_own_operator_line() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let legacy_manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[group_chat]]\nid = \"operator\"\nname = \"Ops Room\"\nmembers = [\"ceo\"]\n",
        )
        .unwrap();
        let state = state_with_manifest(&home, legacy_manifest).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let desks = get_desks(&app, &cookie).await;
        let desks = desks.as_array().unwrap();
        assert_eq!(desks.len(), 1);
        assert_eq!(
            desks[0]["id"], "operator",
            "the desk itself must keep its own literal id: {desks:?}"
        );

        let channel = get_operator_channel(&app, &cookie).await;
        assert_eq!(
            channel["id"],
            crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK,
            "the pinned Operator row must not claim the literal `operator` id \
             once a desk already holds it — otherwise the console shows two \
             rows sharing one id and `findChannel` always resolves the pinned \
             row to the desk: {channel:?}"
        );
    }

    /// Issue #65: the console's default thread addresses sends with
    /// `chat: "main"`, but pre-threading history and the synthetic operator
    /// desk are keyed on `"General"`. A transcript spanning both ids — one
    /// operator turn journaled under each — must read back as one history via
    /// the REST route with no `?desk=` selector (the console's default read).
    #[tokio::test]
    async fn chat_history_route_reunifies_general_and_main_transcripts() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

        runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::AgentReply {
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: "General".to_string(),
                    agent_id: "ceo".to_string(),
                    text: "reply under General".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .unwrap();
        runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::AgentReply {
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: "main".to_string(),
                    agent_id: "ceo".to_string(),
                    text: "reply under main".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .unwrap();

        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/chat/history")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let messages = value.as_array().unwrap();
        let texts: Vec<&str> = messages
            .iter()
            .map(|m| m["text"].as_str().unwrap())
            .collect();
        assert!(
            texts.contains(&"reply under General"),
            "missing General-id reply: {texts:?}"
        );
        assert!(
            texts.contains(&"reply under main"),
            "missing main-id reply: {texts:?}"
        );
    }

    /// Regression: a reply's tool-call timeline must survive a history reload —
    /// switching threads and coming back reloads `chat/history`, which used to
    /// return text only, so the steps vanished. They are now persisted on the
    /// `AgentReply` and projected back through the DTO.
    #[tokio::test]
    async fn chat_history_route_rehydrates_reply_steps() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

        runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::AgentReply {
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: "main".to_string(),
                    agent_id: "ceo".to_string(),
                    text: "done".to_string(),
                    steps: vec![TurnStep {
                        kind: crate::ports::types::TurnStepKind::ToolCall,
                        status: crate::ports::types::TurnStepStatus::Ok,
                        label: "Reading messages".to_string(),
                        detail: None,
                        elapsed_ms: Some(9),
                        ..TurnStep::default()
                    }],
                },
            )
            .await
            .unwrap();

        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/chat/history")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let reply = value
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["text"] == "done")
            .expect("the reply is in history");
        assert_eq!(
            reply["steps"][0]["label"], "Reading messages",
            "the persisted timeline must ride back on the history DTO"
        );
        assert_eq!(reply["steps"][0]["status"], "ok");
        assert_eq!(reply["steps"][0]["elapsedMs"], 9);
    }

    /// Issue #246: a reply that opened a board card must still say so after a
    /// transcript reload. The "card opened" chip is rendered from `taskId`, and
    /// a chip that exists only on the live POST response vanishes the moment
    /// the operator switches threads and comes back — which is exactly when
    /// they would go looking for it.
    #[tokio::test]
    async fn chat_history_route_rehydrates_the_card_a_reply_opened() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

        // The card has to actually be on the board: the history projection
        // reports `taskId` only for a card that still exists, so that a chip
        // cannot come back pointing at a card someone deleted (issue #984).
        runtime
            .tasks()
            .upsert(
                runtime.id(),
                &crate::ports::tasks::TaskRecord {
                    id: "t-77".to_string(),
                    title: TaskTitle::authored("Draft the launch note"),
                    note: None,
                    column: crate::ports::tasks::COLUMN_TODO.to_string(),
                    priority: "medium".to_string(),
                    assignee: String::new(),
                    updated_at_millis: 1,
                    origin: None,
                    parent_task_id: None,
                    output: None,
                    plan: None,
                    planning_attempts: Vec::new(),
                    deliverable: crate::ports::tasks::TaskDeliverable::Once,
                    workflow_proposal: None,
                    origin_run_id: None,
                    origin_workflow_id: None,
                    origin_message_seq: None,
                    bounced: None,
                },
            )
            .await
            .unwrap();

        for (text, task_id) in [
            ("opened one", Some("t-77".to_string())),
            ("just talking", None),
        ] {
            runtime
                .events()
                .append(
                    runtime.id(),
                    CompanyEvent::AgentReply {
                        mentions: Vec::new(),
                        mention_depth: 0,
                        parent: None,
                        task_id,
                        chat_id: "main".to_string(),
                        agent_id: "ceo".to_string(),
                        text: text.to_string(),
                        steps: Vec::new(),
                    },
                )
                .await
                .unwrap();
        }

        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/chat/history")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let messages = value.as_array().unwrap();

        let opened = messages
            .iter()
            .find(|m| m["text"] == "opened one")
            .expect("the card-opening reply is in history");
        assert_eq!(
            opened["taskId"], "t-77",
            "the chip's correlation key must ride back on the history DTO"
        );

        // A reply that opened nothing omits the key rather than sending null,
        // so no bubble grows a chip it should not have — and every message
        // journaled before this field existed reads back unchanged.
        let chatter = messages
            .iter()
            .find(|m| m["text"] == "just talking")
            .expect("the ordinary reply is in history");
        assert!(
            chatter.get("taskId").is_none(),
            "an ordinary chat reply must not carry a card: {chatter}"
        );
    }

    /// A desk id with no `?desk=` selector defaults to the operator/General
    /// thread; an unaddressed thread id that neither matches a manifest desk
    /// nor the General desk reads back empty rather than erroring.
    #[tokio::test]
    async fn chat_history_route_unknown_desk_is_empty_not_an_error() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/chat/history?desk=strategy")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 0);
    }

    /// Issue #862: REST history carries the same cursor/window contract as the
    /// paginated GraphQL surface. A copilot replay can therefore ask for the
    /// tail it needs without the route reading past its cursor.
    #[tokio::test]
    async fn chat_history_route_honors_before_and_limit() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let mut seqs = Vec::new();
        for text in ["oldest", "kept", "newest"] {
            seqs.push(
                runtime
                    .events()
                    .append(
                        runtime.id(),
                        CompanyEvent::AgentReply {
                            mentions: Vec::new(),
                            mention_depth: 0,
                            parent: None,
                            task_id: None,
                            chat_id: "workflow-copilot:weekly_report".to_string(),
                            agent_id: "ceo".to_string(),
                            text: text.to_string(),
                            steps: Vec::new(),
                        },
                    )
                    .await
                    .unwrap(),
            );
        }

        let uri = format!(
            "/api/v1/company/chat/history?desk=workflow-copilot:weekly_report&before={}&limit=1",
            seqs[2].value()
        );

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value[0]["text"], "kept");
        assert_eq!(value.as_array().unwrap().len(), 1);
    }

    /// A history cursor pages messages, not the current reaction state. A
    /// toggle can be journaled after the cursor for a message still selected by
    /// that cursor, and must therefore remain visible on the paged result.
    #[tokio::test]
    async fn chat_history_cursor_keeps_later_reactions_on_displayed_messages() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let message = runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::AgentReply {
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: "General".to_string(),
                    agent_id: "ceo".to_string(),
                    text: "kept".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .unwrap();
        let cursor = runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::FeedbackFiled {
                    note: "cursor marker".to_string(),
                },
            )
            .await
            .unwrap();
        runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::ReactionToggled {
                    message_seq: message,
                    emoji: "👍".to_string(),
                    on: true,
                    by: None,
                },
            )
            .await
            .unwrap();

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/company/chat/history?before={}&limit=1",
                        cursor.value()
                    ))
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value[0]["id"], message.value().to_string());
        assert_eq!(value[0]["reactions"][0]["emoji"], "👍");
    }

    /* ---- issue #364: durable ids, threads, reactions, channel isolation ---- */

    /// Posts a chat message and returns the decoded `ChatResponse` body.
    async fn post_chat(app: &Router, cookie: &str, body: &str) -> serde_json::Value {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "chat POST failed");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Reads a desk's history. `desk` empty reads the default General thread.
    async fn get_history(app: &Router, cookie: &str, desk: &str) -> Vec<serde_json::Value> {
        let uri = if desk.is_empty() {
            "/api/v1/company/chat/history".to_string()
        } else {
            format!("/api/v1/company/chat/history?desk={desk}")
        };
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value.as_array().cloned().unwrap_or_default()
    }

    /// Sets or clears one reaction, returning the status.
    async fn post_reaction(
        app: &Router,
        cookie: &str,
        seq: &str,
        emoji: &str,
        on: bool,
    ) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/company/chat/messages/{seq}/reactions"))
                    .header("cookie", cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "emoji": emoji, "on": on }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    /// The enabler for everything else in #364: a sent message comes back with
    /// the durable id it was journaled under, on both halves of the exchange —
    /// the operator's own line and each reply — and those ids are the same ones
    /// `chat/history` returns on the next read.
    #[tokio::test]
    async fn chat_response_carries_durable_message_ids() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let sent = post_chat(&app, &cookie, r#"{"text":"hi"}"#).await;
        let mine = sent["messageId"].as_str().expect("own message id");
        let reply = sent["responses"][0]["messageId"]
            .as_str()
            .expect("reply message id");
        assert_ne!(mine, reply, "the two halves are separate journal lines");

        let history = get_history(&app, &cookie, "").await;
        let ids: Vec<&str> = history.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&mine), "own id absent from history: {ids:?}");
        assert!(
            ids.contains(&reply),
            "reply id absent from history: {ids:?}"
        );
    }

    /// `detach: true` answers `202` with the ids the accept already established,
    /// claims nothing about a turn that has not settled, and — the half that
    /// removes the 504 from the operator's path — arrives while the turn is
    /// demonstrably still going (issue #983).
    #[tokio::test]
    async fn a_detached_turn_answers_202_before_the_turn_finishes() {
        let home_dir = home();
        // The same blocking brain the queue tests use: it parks inside the cycle
        // until released, so the turn is provably unfinished when the response
        // below is read.
        let (brain, entered, release) = BlockingChatBrain::new();
        let state = build_state_with_brain(
            home_dir.path(),
            "running",
            AppConfig::default(),
            Some(brain),
        )
        .await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let app = router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"do the long thing","detach":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(body["detached"], true, "{body}");
        assert!(
            body["turnId"].as_str().is_some_and(|s| !s.is_empty()),
            "the turn id is what the console polls: {body}"
        );
        assert!(
            body["messageId"].as_str().is_some_and(|s| !s.is_empty()),
            "the message is journaled at accept, so its id is knowable here: {body}"
        );
        // The whole point: this body is not allowed to look settled. A console
        // that found `responses` here would render an empty answer as the reply.
        assert!(
            body.get("responses").is_none(),
            "a detached response must not look settled: {body}"
        );
        assert!(
            body.get("stillAwaiting").is_none(),
            "a detached response must not look settled: {body}"
        );

        // And the turn really had not finished when that body was written — the
        // brain is still parked, holding the cycle open.
        entered.acquire().await.expect("the turn entered").forget();
        let statuses: Vec<String> = turn_rows(&runtime)
            .await
            .into_iter()
            .map(|(_, status)| status)
            .collect();
        assert!(
            statuses.iter().any(|s| s == "running" || s == "pending"),
            "the response beat the turn, which is the point: {statuses:?}"
        );

        // It settles on its own, with nobody waiting on it.
        release.add_permits(1);
        until("the detached turn never settled", async || {
            turn_rows(&runtime)
                .await
                .iter()
                .any(|(_, status)| status == "succeeded")
        })
        .await;
    }

    /// The wire-compat guarantee in the other direction: a caller that sends no
    /// `detach` gets exactly the response it always got — a `200` carrying the
    /// settled turn — plus the additive `turnId`. An older console is untouched.
    #[tokio::test]
    async fn a_body_without_detach_still_gets_the_synchronous_response() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        // `post_chat` asserts the 200 itself — the legacy status is part of what
        // this test is pinning.
        let body = post_chat(&app, &cookie, r#"{"text":"hi"}"#).await;

        assert!(
            body["responses"].as_array().is_some_and(|r| !r.is_empty()),
            "the settled shape carries the replies: {body}"
        );
        assert!(
            body["messageId"].as_str().is_some(),
            "the legacy durable id is unchanged: {body}"
        );
        assert!(
            body.get("detached").is_none(),
            "the synchronous response must not carry the detach discriminator: {body}"
        );
        assert!(
            body["turnId"].as_str().is_some(),
            "`turnId` is additive on the synchronous response too: {body}"
        );
    }

    /// The detached turn is not fire-and-forget: the message it journaled at
    /// accept, and the answer the spawned task journals afterwards, both land in
    /// the durable transcript. This is the backstop the console re-reads, and
    /// the reason a dropped frame is not a lost answer.
    #[tokio::test]
    async fn a_detached_turn_still_journals_its_question_and_its_answer() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"detached hello","detach":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let mine = body["messageId"].as_str().unwrap().to_string();

        // The turn owns its own settle, so wait for the answer to appear rather
        // than for a handle this route deliberately does not hold.
        let mut history = Vec::new();
        for _ in 0..100 {
            history = get_history(&app, &cookie, "").await;
            if history
                .iter()
                .any(|m| m["text"].as_str() == Some("You said: detached hello"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let ids: Vec<&str> = history.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert!(
            ids.contains(&mine.as_str()),
            "the id handed back at 202 must resolve in history: {ids:?}"
        );
        assert!(
            history
                .iter()
                .any(|m| m["text"].as_str() == Some("You said: detached hello")),
            "the detached turn's answer never reached the transcript: {history:?}"
        );
    }

    /// **Issue #1000 — the floor of the `202` contract.** A detached response
    /// is a promise the console can poll, and the poll starts from the body's
    /// `turnId`; a `202` carrying no row is a promise a buffered-`/events`
    /// tenant cannot collect, which strands the reply until reload. So when the
    /// turn's row cannot be minted, the route must not answer `202` at all: it
    /// settles the turn synchronously instead, handing the console the answer —
    /// a state the console renders natively, being the same shape an older host
    /// (one that ignored `detach`) has always returned.
    #[tokio::test]
    async fn a_detached_request_without_a_turn_row_settles_synchronously() {
        let home_dir = home();
        let state = state_with_failing_runs(home_dir.path()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let body = post_chat(&app, &cookie, r#"{"text":"rowless detach","detach":true}"#).await;

        // The settled shape, not the empty 202: the console is handed the
        // answer, never a turn id it cannot act on.
        assert!(
            body.get("detached").is_none(),
            "a rowless turn must not claim it can be read back: {body}"
        );
        assert!(
            body["responses"].as_array().is_some_and(|r| !r.is_empty()),
            "the synchronous fallback still delivers the reply: {body}"
        );
    }

    /// A thread reply survives a reload: the parent id posted with the message
    /// comes back on both the operator's line and the answer it drew, so a
    /// rehydrating console folds the exchange under the same row it was typed
    /// under instead of flattening it into the channel.
    #[tokio::test]
    async fn thread_replies_survive_a_history_reload() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let root = post_chat(&app, &cookie, r#"{"text":"the plan"}"#).await;
        let root_id = root["messageId"].as_str().unwrap().to_string();

        let threaded = post_chat(
            &app,
            &cookie,
            &serde_json::json!({ "text": "a follow-up", "parent": root_id }).to_string(),
        )
        .await;
        assert!(
            threaded["responses"][0]["messageId"]
                .as_str()
                .is_some_and(|id| id != root_id),
            "the answer is its own journal line, not the root's"
        );

        let history = get_history(&app, &cookie, "").await;
        let parented: Vec<(&str, Option<&str>)> = history
            .iter()
            .map(|m| (m["text"].as_str().unwrap(), m["parentId"].as_str()))
            .collect();
        // The root sits in the channel; both halves of the threaded exchange
        // hang off it — the answer under the row the thread opened from, not
        // under the question, so a thread never nests inside a thread.
        assert!(
            parented.contains(&("the plan", None)),
            "root should be unparented: {parented:?}"
        );
        assert!(
            parented.contains(&("a follow-up", Some(root_id.as_str()))),
            "threaded message lost its parent: {parented:?}"
        );
        assert!(
            parented
                .iter()
                .any(|(text, parent)| *text == "You said: a follow-up"
                    && *parent == Some(root_id.as_str())),
            "the reply to a threaded message left the thread: {parented:?}"
        );
    }

    /// A parent that is not a message id is a 400, not a silently-flattened
    /// thread: a reply that quietly lands in the channel reads to the operator
    /// as a reply that went missing.
    #[tokio::test]
    async fn chat_rejects_a_parent_that_is_not_a_message_id() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi","parent":"m3"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// A reaction records who reacted and survives a reload; clearing it removes
    /// the row; and setting the same reaction twice leaves exactly one row —
    /// the explicit `on` flag is what makes the write idempotent.
    #[tokio::test]
    async fn reactions_persist_are_attributed_and_are_idempotent() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let sent = post_chat(&app, &cookie, r#"{"text":"ship it"}"#).await;
        let target = sent["messageId"].as_str().unwrap().to_string();

        assert_eq!(
            post_reaction(&app, &cookie, &target, "👍", true).await,
            StatusCode::NO_CONTENT
        );
        // Twice, deliberately: a retry or a double tap must not double the row.
        assert_eq!(
            post_reaction(&app, &cookie, &target, "👍", true).await,
            StatusCode::NO_CONTENT
        );

        let history = get_history(&app, &cookie, "").await;
        let reacted = history
            .iter()
            .find(|m| m["id"].as_str() == Some(target.as_str()))
            .expect("the reacted-to message is still in history");
        let rows = reacted["reactions"].as_array().expect("reactions present");
        assert_eq!(rows.len(), 1, "one row per person per emoji: {rows:?}");
        assert_eq!(rows[0]["emoji"], "👍");
        assert_eq!(rows[0]["mine"], true, "the reader is the one who reacted");
        assert!(
            rows[0]["by"].as_str().is_some_and(|by| !by.is_empty()),
            "a reaction names who made it: {rows:?}"
        );

        // Clearing drops the row entirely rather than leaving a zero behind.
        assert_eq!(
            post_reaction(&app, &cookie, &target, "👍", false).await,
            StatusCode::NO_CONTENT
        );
        let history = get_history(&app, &cookie, "").await;
        let cleared = history
            .iter()
            .find(|m| m["id"].as_str() == Some(target.as_str()))
            .unwrap();
        assert!(
            cleared.get("reactions").is_none(),
            "a cleared reaction leaves no row: {cleared:?}"
        );
    }

    /// A reaction may only name a chat message. A sequence position that holds
    /// something else — or nothing at all — is a 404, so the log can never carry
    /// a reaction no reader could render.
    #[tokio::test]
    async fn reactions_refuse_a_target_that_is_not_a_message() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let not_a_message = runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::FeedbackFiled {
                    note: "unrelated".to_string(),
                },
            )
            .await
            .unwrap();
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        assert_eq!(
            post_reaction(
                &app,
                &cookie,
                &not_a_message.value().to_string(),
                "👍",
                true
            )
            .await,
            StatusCode::NOT_FOUND
        );
        // A sequence position nothing has ever occupied.
        assert_eq!(
            post_reaction(&app, &cookie, "99999", "👍", true).await,
            StatusCode::NOT_FOUND
        );
        // And a target that is not a sequence position at all.
        assert_eq!(
            post_reaction(&app, &cookie, "m3", "👍", true).await,
            StatusCode::BAD_REQUEST
        );
    }

    /// A reaction is a journal line read by the operator projection, so it takes
    /// an emoji and not a payload: empty, oversized, and control-character
    /// bodies are all refused.
    #[tokio::test]
    async fn reactions_refuse_a_body_that_is_not_an_emoji() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let sent = post_chat(&app, &cookie, r#"{"text":"hi"}"#).await;
        let target = sent["messageId"].as_str().unwrap().to_string();

        for bad in ["", "   ", "yes\nno", &"x".repeat(REACTION_MAX_BYTES + 1)] {
            assert_eq!(
                post_reaction(&app, &cookie, &target, bad, true).await,
                StatusCode::BAD_REQUEST,
                "accepted a non-emoji reaction: {bad:?}"
            );
        }
        // A multi-code-point emoji is still one reaction, and is accepted.
        assert_eq!(
            post_reaction(&app, &cookie, &target, "👩‍💻", true).await,
            StatusCode::NO_CONTENT
        );
    }

    /// PR #1781 review: `history_for_desk` (reload) and `project_event_for_viewer`
    /// (live SSE) both already hide an owner-fallback report from a non-admin —
    /// this proves the reaction route agrees, rather than letting a Member
    /// react to (and thereby confirm the existence and sequence position of) a
    /// report they cannot read. Answered with the same 404 an unknown sequence
    /// gets, not a 403, so probing this endpoint cannot distinguish "hidden"
    /// from "never existed".
    #[tokio::test]
    async fn reactions_refuse_a_target_that_is_an_admin_only_report() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let report = runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::AgentReply {
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: "operator".into(),
                    agent_id: crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR.to_string(),
                    text: "no admin has a mailbox".into(),
                    steps: Vec::new(),
                },
            )
            .await
            .unwrap();
        crate::server::test_support::seed_fixed_member(&state, "acme").await;
        let app = router(state);
        let member_cookie = crate::server::test_support::member_cookie("acme");
        let admin_cookie = crate::server::test_support::fixed_cookie("acme");

        // A Member gets the same 404 an unknown message would.
        assert_eq!(
            post_reaction(
                &app,
                &member_cookie,
                &report.value().to_string(),
                "👍",
                true
            )
            .await,
            StatusCode::NOT_FOUND
        );
        // An admin may react to it normally.
        assert_eq!(
            post_reaction(&app, &admin_cookie, &report.value().to_string(), "👍", true).await,
            StatusCode::NO_CONTENT
        );
    }

    /// Regression for the third acceptance item of #364, which the console's
    /// own scoping already satisfied but nothing pinned: a message posted in one
    /// channel must be absent from another, end to end through the route — not
    /// only in the `owns` predicate. Reactions ride the same boundary.
    #[tokio::test]
    async fn a_message_in_one_channel_is_absent_from_another() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, desk_manifest()).await;
        let app = router(state);
        let cookie = crate::server::test_support::fixed_cookie("acme");

        let in_studio = post_chat(&app, &cookie, r#"{"text":"studio only","chat":"studio"}"#).await;
        let studio_id = in_studio["messageId"].as_str().unwrap().to_string();
        post_chat(&app, &cookie, r#"{"text":"general only"}"#).await;
        assert_eq!(
            post_reaction(&app, &cookie, &studio_id, "👀", true).await,
            StatusCode::NO_CONTENT
        );

        let studio: Vec<String> = get_history(&app, &cookie, "studio")
            .await
            .iter()
            .map(|m| m["text"].as_str().unwrap().to_string())
            .collect();
        let general = get_history(&app, &cookie, "").await;
        let general_texts: Vec<&str> = general
            .iter()
            .map(|m| m["text"].as_str().unwrap())
            .collect();

        assert!(
            studio.iter().any(|t| t == "studio only"),
            "the desk lost its own message: {studio:?}"
        );
        assert!(
            !studio.iter().any(|t| t == "general only"),
            "a General message leaked into the desk: {studio:?}"
        );
        assert!(
            general_texts.contains(&"general only"),
            "General lost its own message: {general_texts:?}"
        );
        assert!(
            !general_texts.contains(&"studio only"),
            "a desk message leaked into General: {general_texts:?}"
        );
        // The reaction is on the desk's message, so it is not visible from a
        // channel that cannot see the message it is about.
        assert!(
            general.iter().all(|m| m.get("reactions").is_none()),
            "a reaction crossed a channel boundary: {general:?}"
        );
    }

    #[tokio::test]
    async fn chat_by_id_matches_registered_company() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/companies/acme/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"yo"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_company_is_404() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/companies/ghost/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // 401, not 404: the caller holds no credential for `ghost`, and
        // authentication precedes existence. Answering "no such company" to an
        // unauthenticated caller would let anyone enumerate which companies a
        // host runs. A user of `ghost` gets a real 404.
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn paused_company_chat_is_409() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "paused").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn list_and_status_routes_report_the_company() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let list = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        let bytes = to_bytes(list.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 1);
        assert_eq!(value[0]["id"], "acme");

        let status = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies/acme")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let bytes = to_bytes(status.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["id"], "acme");
    }

    #[tokio::test]
    async fn approvals_list_is_empty_before_any_park() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/approvals")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn amended_approve_resolves_and_returns_responses() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        // An `approve` verdict carrying an amended payload routes to the
        // approve-with-edit path. Even against an unknown id it resolves
        // cleanly (nothing to execute) and the follow-up cycle replies.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/approvals/missing")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"verdict":"approve","amended_payload":{"text":"edited"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value["responses"].is_array());
    }

    /// The tool call the operator is asked to sign off. `agent: Some(_)` is what
    /// makes approving it mint a single-use grant rather than execute it
    /// (issue #243) — which is the whole reason a lost continuation hurts: the
    /// grant is spent on a turn that never happens.
    ///
    /// The payload names an action the vendored catalogue tags `Write`, so
    /// `consequence_of` classifies it as a send on its merits. Until issue #470
    /// it named the slug under `tool_slug`, a key neither the tool nor the
    /// classifier reads — so it was a call with no action at all, and it
    /// reached the per-call verdict through the unknown-slug fallback instead.
    fn gated_tool_call() -> crate::ports::types::Effect {
        crate::ports::types::Effect {
            kind: "composio_execute".into(),
            group: crate::ports::types::EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: crate::policy::test_support::composio_send_args(),
            agent: Some("ceo".into()),
            run_id: None,
        }
    }

    /// A tool call an operator MAY grant a standing permission for (issue
    /// #431), which `gated_tool_call` deliberately is not: its Composio payload
    /// names an action the catalogue tags `Write`, so `consequence_of` reads it
    /// as a send and it stays a per-call decision. `file_write` is declared
    /// grantable in `src/policy/consequence.rs` and carries an agent, so it
    /// satisfies both halves of `check_broadly_grantable` — it mutates, but
    /// only the agent's own sandboxed workspace.
    fn grantable_tool_call() -> crate::ports::types::Effect {
        crate::ports::types::Effect {
            kind: "file_write".into(),
            group: crate::ports::types::EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "path": "notes/a.md", "body": "one" }),
            agent: Some("ceo".into()),
            run_id: None,
        }
    }

    /// Issue #618: membership gets you the approval, role gets you its
    /// contents.
    ///
    /// Issue #561: the receipt says whether this decision actually released the
    /// turn.
    ///
    /// A turn that parked two calls is blocked on two decisions (issue #469
    /// continues it once, on the last one). The console used to tell the
    /// operator "the agent is completing the action" on the first click, which
    /// is false — nothing runs until the second. This is the count it now words
    /// that sentence from: one still owed after the first decision, none after
    /// the second.
    #[tokio::test]
    async fn a_receipt_says_how_many_decisions_the_turn_is_still_blocked_on() {
        use crate::runtime::journal::{ApprovalConversation, TaskLink};

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

        let effect = |memo: &str| crate::ports::types::Effect {
            kind: "payment.send".into(),
            group: crate::ports::types::EffectGroup::Spend,
            amount_usd: Some(10.0),
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "to": "board@example.test", "memo": memo }),
            agent: Some("ceo".into()),
            run_id: None,
        };

        // One turn, two parked calls — the shape an operator meets whenever an
        // agent gates more than once in a turn.
        for (id, memo) in [("appr-561-a", "first"), ("appr-561-b", "second")] {
            runtime
                .journal
                .record_parked(
                    &crate::ports::types::ApprovalId::new(id),
                    &effect(memo),
                    1_000,
                    TaskLink::Unlinked,
                    ApprovalConversation::default(),
                    Some("cycle-561".to_string()),
                )
                .await
                .unwrap();
            runtime.continuations.arm("cycle-561");
        }

        let app = router(state);
        let resolve = |app: axum::Router, id: &'static str| async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/api/v1/company/approvals/{id}"))
                        .header("content-type", "application/json")
                        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                        .body(Body::from(
                            serde_json::json!({ "verdict": "approve", "detach": true }).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };

        let first = resolve(app.clone(), "appr-561-a").await;
        assert_eq!(
            first["stillAwaiting"], 1,
            "the first decision releases nothing — the turn is still blocked on the second: {first}"
        );

        // The count is read per decision, at the moment the verdict lands. What
        // happens to the sibling afterwards is issue #848's business — a turn's
        // gated calls may be consolidated and settle together — and this test
        // deliberately asserts only the half the operator's confirmation is
        // worded from: this click did not release the turn.
        //
        // The other half — the last decision reporting nothing outstanding —
        // is pinned on the queue itself in
        // `runtime::continuation::test::outstanding_counts_the_decision_being_made`,
        // where it is deterministic rather than racing a spawned follow-up.
    }

    /// **The two-account part is the point.** The harness signs every request
    /// in as an admin, so a redaction verified only as an admin passes
    /// identically against no redaction at all — the test would prove nothing
    /// while looking like coverage. This seeds a second, Member-role account
    /// and drives the same route with both.
    #[tokio::test]
    async fn a_member_sees_the_approval_but_not_its_payload_or_amount() {
        use crate::runtime::journal::{ApprovalConversation, TaskLink};

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        crate::server::test_support::seed_fixed_member(&state, "acme").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

        let effect = crate::ports::types::Effect {
            kind: "payment.send".into(),
            group: crate::ports::types::EffectGroup::Spend,
            amount_usd: Some(2400.0),
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "to": "board@example.test", "memo": "Q3 retainer" }),
            agent: Some("ceo".into()),
            run_id: None,
        };
        runtime
            .journal
            .record_parked(
                &crate::ports::types::ApprovalId::new("appr-618"),
                &effect,
                1_000,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();

        let app = router(state);

        async fn approvals_as(app: &axum::Router, cookie: String) -> serde_json::Value {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/company/approvals")
                        .header("cookie", cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }

        // The admin decides the sign-off, so the admin sees what it will do.
        let as_admin = approvals_as(&app, crate::server::test_support::fixed_cookie("acme")).await;
        let admin_row = &as_admin.as_array().unwrap()[0];
        assert_eq!(admin_row["amount_usd"].as_f64(), Some(2400.0));
        assert_eq!(admin_row["payload"]["to"], "board@example.test");
        assert!(
            admin_row.get("contents_hidden").is_none(),
            "an admin is not told anything was hidden: {admin_row}"
        );

        let as_member =
            approvals_as(&app, crate::server::test_support::member_cookie("acme")).await;
        let member_row = &as_member.as_array().unwrap()[0];

        // Still visible: everything that makes stalled work legible. This half
        // is what #468 depends on — a member must keep seeing that work is
        // waiting and what kind of call it is.
        assert_eq!(member_row["id"], "appr-618");
        assert_eq!(member_row["kind"], "payment.send");
        assert_eq!(member_row["agent"], "ceo");
        assert_eq!(member_row["at_millis"].as_u64(), Some(1_000));

        // Withheld: the recipient and the money.
        assert!(
            member_row.get("payload").is_none(),
            "the recipient must not reach a member: {member_row}"
        );
        // `null`, not absent: unlike `payload`, `amount_usd` carries no
        // `skip_serializing_if`, so it stays on the wire as an explicit null.
        // Both read as "no value" to the console (`a.amount_usd != null`
        // covers either), and changing the wire shape as a side effect of a
        // redaction would be a worse trade than asserting the shape that is
        // actually there.
        assert!(
            member_row["amount_usd"].is_null(),
            "nor the amount: {member_row}"
        );
        assert_eq!(
            member_row["contents_hidden"], true,
            "and the console must be able to say so rather than render an empty card: {member_row}"
        );

        // Belt and braces: the recipient string must appear nowhere in the
        // member's response, however the shape changes later.
        let raw = serde_json::to_string(&as_member).unwrap();
        assert!(
            !raw.contains("board@example.test") && !raw.contains("Q3 retainer"),
            "payload content leaked to a member: {raw}"
        );
    }

    /// The dotted kind the stalled brain parks once its follow-up turn gets
    /// past the barrier. Parking journals durably (`record_parked`), so its
    /// presence in `pending_approvals()` is proof the continuation reached the
    /// end of the turn *and* wrote to disk — not merely that a task was alive.
    const CONTINUATION_MARKER: &str = "continuation.marker";

    /// A brain that parks one gated tool call per operator message and, on the
    /// follow-up `ApprovalResolved` cycle, blocks mid-turn until the test
    /// releases it — the shape of a slow agent turn behind a proxy.
    struct StalledContinuationBrain {
        /// Fires once the follow-up turn has begun. By this point the verdict
        /// is journaled and the grant minted, so this is exactly the moment the
        /// field report's connection died.
        entered: Arc<tokio::sync::Notify>,
        /// The test's permission for the turn to finish.
        release: Arc<tokio::sync::Notify>,
        /// The effect parked for the operator's sign-off. Whether it may be
        /// granted a standing permission is a property of this effect, so the
        /// scope tests supply their own rather than sharing one fixture.
        parked: crate::ports::types::Effect,
    }

    #[async_trait::async_trait]
    impl crate::ports::brain::Brain for StalledContinuationBrain {
        async fn run_cycle(
            &self,
            req: crate::ports::types::CycleRequest,
            host: &dyn crate::ports::brain::CycleHost,
        ) -> crate::Result<crate::ports::types::CycleResult> {
            for event in &req.events {
                match event {
                    CompanyEvent::OperatorMessage { .. } => {
                        host.park_effect(self.parked.clone()).await?;
                    }
                    CompanyEvent::ApprovalResolved { .. } => {
                        self.entered.notify_one();
                        self.release.notified().await;
                        host.park_effect(crate::ports::types::Effect {
                            kind: CONTINUATION_MARKER.into(),
                            group: crate::ports::types::EffectGroup::Other,
                            amount_usd: None,
                            established_thread: false,
                            first_time_counterparty: false,
                            payload: serde_json::json!({}),
                            agent: None,
                            run_id: None,
                        })
                        .await?;
                    }
                    _ => {}
                }
            }
            Ok(crate::ports::types::CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![crate::ports::types::CompressedTrace::now(
                    &req.cycle_id,
                    "stalled continuation",
                )],
                ledger_deltas: Vec::new(),
                token_usage: crate::ports::types::TokenUsage::default(),
            })
        }
    }

    fn chat_request(text: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/company/chat")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({ "text": text }).to_string()))
            .unwrap()
    }

    /// A resolve against the single-company alias. `scope` lets the same body be
    /// aimed at the `/companies/{id}` form, which must behave identically.
    fn resolve_request_scoped(
        scope: &str,
        approval_id: &ApprovalId,
        body: serde_json::Value,
    ) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("{scope}/approvals/{approval_id}"))
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn resolve_request(approval_id: &ApprovalId, body: serde_json::Value) -> Request<Body> {
        resolve_request_scoped("/api/v1/company", approval_id, body)
    }

    // -- Extend the deadline (issue #1805) -----------------------------------

    /// Parks one effect in BOTH the gate and the journal under a fixed id, at a
    /// controllable instant — the gate is what `extend_approval` asks whether an
    /// id is live, and the journal is what projects the deadline, so an extend
    /// test needs both seeded exactly as a real park leaves them.
    async fn park_for_extend(
        runtime: &Arc<CompanyRuntime>,
        id: &str,
        at_millis: u64,
    ) -> ApprovalId {
        use crate::runtime::journal::{ApprovalConversation, TaskLink};
        let approval = ApprovalId::new(id);
        let effect = crate::ports::types::Effect {
            kind: "payment.send".into(),
            group: crate::ports::types::EffectGroup::Spend,
            amount_usd: Some(1_200.0),
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "to": "vendor@example.test" }),
            agent: Some("ceo".into()),
            run_id: None,
        };
        runtime
            .approval_gate
            .rehydrate(approval.clone(), effect.clone(), at_millis);
        runtime
            .journal
            .record_parked(
                &approval,
                &effect,
                at_millis,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        approval
    }

    fn extend_request_with_cookie(approval_id: &ApprovalId, cookie: String) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/company/approvals/{approval_id}/extend"))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    }

    fn extend_request(approval_id: &ApprovalId) -> Request<Body> {
        extend_request_with_cookie(
            approval_id,
            crate::server::test_support::fixed_cookie("acme"),
        )
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The keystone (issue #1805): extending a parked approval pushes its
    /// deadline out to a fresh full window, and the receipt names the new one —
    /// the console can redraw the countdown without re-fetching the list.
    #[tokio::test]
    async fn extending_a_parked_approval_moves_its_deadline() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        // Parked long ago, so its original deadline is `1_000 + ttl`.
        let id = park_for_extend(&runtime, "appr-ext", 1_000).await;
        let before = runtime.pending_approvals()[0]
            .expires_at_millis
            .expect("a deadline is projected");

        let app = router(state);
        let response = app.oneshot(extend_request(&id)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;

        let after = runtime.pending_approvals()[0]
            .expires_at_millis
            .expect("a deadline is still projected");
        assert!(
            after > before,
            "the deadline moved out: before={before} after={after}"
        );
        assert!(body["extended"].as_bool().unwrap());
        assert_eq!(
            body["expiresAtMillis"].as_f64().unwrap() as u64,
            after,
            "the receipt's deadline is the one the card now projects"
        );
    }

    /// Extending something that is not parked — an unknown id, or one already
    /// resolved or expired — is a 404, not a 200 over nothing.
    #[tokio::test]
    async fn extending_an_unknown_approval_is_404() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let app = router(state);
        let response = app
            .oneshot(extend_request(&ApprovalId::new("does-not-exist")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// The route is guarded by the same company auth as resolve: a member — an
    /// authenticated user of the company — may extend a deadline, exactly as
    /// they may resolve. Keeping a stalled run alive is not an admin-only lever.
    #[tokio::test]
    async fn a_member_may_extend_an_approval_deadline() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        crate::server::test_support::seed_fixed_member(&state, "acme").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let id = park_for_extend(&runtime, "appr-member-ext", 1_000).await;

        let app = router(state);
        let response = app
            .oneshot(extend_request_with_cookie(
                &id,
                crate::server::test_support::member_cookie("acme"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Whether the stalled brain's follow-up turn has journaled its marker yet.
    fn continued(runtime: &Arc<CompanyRuntime>) -> bool {
        runtime
            .pending_approvals()
            .iter()
            .any(|a| a.kind == CONTINUATION_MARKER)
    }

    /// Waits for the stalled brain's follow-up turn to journal its marker.
    async fn await_continuation(runtime: &Arc<CompanyRuntime>) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !continued(runtime) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    }

    /// A running company with one tool call parked and a brain that will stall
    /// on the follow-up turn until `release` is fired.
    struct StalledCompany {
        app: axum::Router,
        runtime: Arc<CompanyRuntime>,
        approval_id: ApprovalId,
        /// Fires once the follow-up turn has begun — by which point the verdict
        /// is journaled and the grant minted.
        entered: Arc<tokio::sync::Notify>,
        /// The test's permission for that turn to finish.
        release: Arc<tokio::sync::Notify>,
    }

    async fn stalled_company(home: &std::path::Path) -> StalledCompany {
        stalled_company_parking(home, gated_tool_call()).await
    }

    /// `stalled_company`, with the parked effect chosen by the caller — because
    /// whether a scope may be granted is decided by the effect, not the route.
    async fn stalled_company_parking(
        home: &std::path::Path,
        parked: crate::ports::types::Effect,
    ) -> StalledCompany {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let state = build_state_with_brain(
            home,
            "running",
            AppConfig::default(),
            Some(Arc::new(StalledContinuationBrain {
                entered: entered.clone(),
                release: release.clone(),
                parked,
            })),
        )
        .await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let app = router(state);

        let response = app.clone().oneshot(chat_request("do it")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let parked = runtime.pending_approvals();
        assert_eq!(parked.len(), 1, "the brain parked one tool call");
        let approval_id = parked[0].id.clone();

        StalledCompany {
            app,
            runtime,
            approval_id,
            entered,
            release,
        }
    }

    /// **Issue #383 / #380 defect 3 — the keystone.** A client that walks away
    /// mid-turn must not take the agent's continuation with it.
    ///
    /// The host is plain `axum::serve(listener, router(state))` and nothing on
    /// the resolve path was spawned, so the follow-up agent turn lived *inside*
    /// the request future. Hyper drops that future the moment the peer closes,
    /// and nginx closes its upstream connection when it gives up on a slow
    /// response. So on a hosted tenant the sequence was: verdict recorded,
    /// journaled, single-use grant minted — and then the re-dispatch the grant
    /// existed for cancelled mid-flight. The operator's approval was spent and
    /// the conversation never resumed, which is precisely what #380 reported.
    ///
    /// `Router::oneshot` reproduces that cancellation faithfully rather than by
    /// analogy: the mechanism is the same one hyper uses — the handler future is
    /// owned by the future the caller is polling, and dropping the latter drops
    /// the former.
    #[tokio::test]
    async fn a_dropped_connection_does_not_cancel_the_follow_up_cycle() {
        let home_dir = home();
        let c = stalled_company(home_dir.path()).await;

        // Approve it, then let the connection die once the turn is under way.
        let mut resolving = Box::pin(c.app.clone().oneshot(resolve_request(
            &c.approval_id,
            serde_json::json!({"verdict":"approve"}),
        )));
        tokio::select! {
            _ = &mut resolving => panic!("the resolve answered before the follow-up turn began"),
            _ = c.entered.notified() => {}
        }
        drop(resolving);

        // The verdict is already durable and the grant already spent — this is
        // the state the operator is left in when the proxy gives up.
        assert!(
            !c.runtime
                .pending_approvals()
                .iter()
                .any(|a| a.id == c.approval_id),
            "the verdict was journaled before the connection dropped"
        );
        assert!(
            c.runtime.grants.peek(&c.approval_id).is_some(),
            "the single-use grant was minted before the connection dropped"
        );

        // So the continuation the grant exists for must still complete.
        c.release.notify_one();
        assert!(
            await_continuation(&c.runtime).await,
            "the follow-up cycle died with the dropped connection: the grant is spent \
             and the agent never continued"
        );
        assert_eq!(
            c.runtime.grants.live_count(),
            1,
            "the continuation minted no second grant"
        );
    }

    /// The reply a stalled chat turn produces once released.
    const SLOW_TURN_REPLY: &str = "the slow turn's answer";

    /// A brain that stalls on the operator's **first** turn — the chat lane,
    /// rather than the approval follow-up `StalledContinuationBrain` stalls on.
    struct StalledChatBrain {
        /// Fires once the turn is under way, which is the moment the field
        /// report's proxy gave up and closed the connection.
        entered: Arc<tokio::sync::Notify>,
        /// The test's permission for that turn to finish.
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::ports::brain::Brain for StalledChatBrain {
        async fn run_cycle(
            &self,
            req: crate::ports::types::CycleRequest,
            _host: &dyn crate::ports::brain::CycleHost,
        ) -> crate::Result<crate::ports::types::CycleResult> {
            let mut channel_responses = Vec::new();
            for event in &req.events {
                if matches!(event, CompanyEvent::OperatorMessage { .. }) {
                    self.entered.notify_one();
                    self.release.notified().await;
                    channel_responses.push(crate::ports::types::OutboundMessage {
                        message_id: None,
                        task_id: None,
                        channel: "operator".into(),
                        agent: None,
                        text: SLOW_TURN_REPLY.into(),
                        steps: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    });
                }
            }
            Ok(crate::ports::types::CycleResult {
                channel_responses,
                new_traces: vec![crate::ports::types::CompressedTrace::now(
                    &req.cycle_id,
                    "stalled chat",
                )],
                ledger_deltas: Vec::new(),
                token_usage: crate::ports::types::TokenUsage::default(),
            })
        }
    }

    /// Whether the turn's answer reached the durable journal.
    async fn reply_journaled(runtime: &Arc<CompanyRuntime>) -> bool {
        runtime
            .events()
            .read_from(runtime.id(), EventSeq::new(0), 10_000)
            .await
            .unwrap()
            .iter()
            .any(|stored| {
                matches!(
                    &stored.event,
                    CompanyEvent::AgentReply { text, .. } if text == SLOW_TURN_REPLY
                )
            })
    }

    /// Waits for the released turn to journal its reply.
    async fn await_reply_journaled(runtime: &Arc<CompanyRuntime>) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !reply_journaled(runtime).await {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    }

    /// **Issue #882.** A chat turn whose caller walks away mid-flight must still
    /// finish and still journal its answer.
    ///
    /// This is the chat-lane twin of
    /// `a_dropped_connection_does_not_cancel_the_follow_up_cycle`. Both the
    /// cycle and the `AgentReply` append used to live inside the request future,
    /// so a turn slower than nginx's read timeout was cancelled mid-flight and
    /// the answer was never written. The operator's DM history then held their
    /// question and nothing else — the turn could not be read back on reload and
    /// could not be resumed, which is what #882 reported. Workflow runs survived
    /// the identical 504 precisely because they are spawned.
    ///
    /// `Router::oneshot` reproduces the cancellation by the same mechanism hyper
    /// uses: the handler future is owned by the future the caller polls, so
    /// dropping the latter drops the former.
    #[tokio::test]
    async fn a_dropped_connection_does_not_lose_the_chat_turns_work() {
        let home_dir = home();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let state = build_state_with_brain(
            home_dir.path(),
            "running",
            AppConfig::default(),
            Some(Arc::new(StalledChatBrain {
                entered: entered.clone(),
                release: release.clone(),
            })),
        )
        .await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let app = router(state);

        // Send the turn, then let the connection die once it is under way —
        // exactly what the proxy does when it decides the upstream is too slow.
        let mut chatting = Box::pin(app.clone().oneshot(chat_request("run the seo audit")));
        tokio::select! {
            _ = &mut chatting => panic!("the chat answered before the turn began"),
            _ = entered.notified() => {}
        }
        drop(chatting);

        // Nothing is journaled yet: the turn is still stalled inside the brain.
        assert!(
            !reply_journaled(&runtime).await,
            "the reply was journaled before the turn was released"
        );

        // Issue #983: the turn was recorded the instant it was accepted, and
        // the record is what a re-read resolves — so at this point the operator
        // has walked away and the turn is still `Running` rather than absent.
        let row = turn_rows(&runtime)
            .await
            .pop()
            .expect("accepting the turn minted a row");
        assert_eq!(
            row.1, "running",
            "a turn whose caller is gone must still read as under way"
        );

        // The work must survive the caller giving up.
        release.notify_one();
        assert!(
            await_reply_journaled(&runtime).await,
            "the chat turn died with the dropped connection: the operator's \
             message is journaled, the answer is not, and the turn can neither \
             be read back nor resumed (issue #882)"
        );

        // Issue #983: and so must the settle. The row is written by the spawned
        // task, not by the handler, so a dropped connection leaving it
        // `Running` forever would be the #882 bug one layer down — the turn
        // finishes, the answer lands, and the status surface still claims work
        // is in flight until the next boot reaps it.
        until("the settle died with the dropped connection", async || {
            turn_rows(&runtime)
                .await
                .iter()
                .all(|(_, status)| status == "succeeded")
        })
        .await;
    }

    // ── Issue #983: an accepted turn exists and can be read back ────────────

    /// A brain that blocks every operator turn on a semaphore the test holds.
    ///
    /// Deliberately a `Semaphore` rather than a `Notify`: these tests run two
    /// turns at once and release both, and `notify_one` wakes exactly one
    /// waiter while `notify_waiters` wakes only those already parked. Permits
    /// are held whether or not anybody is waiting yet, so the release cannot
    /// race the turns into a hang.
    struct BlockingChatBrain {
        /// One permit added per turn that has entered the brain.
        entered: Arc<tokio::sync::Semaphore>,
        /// The test's permission for a turn to finish — one permit each.
        release: Arc<tokio::sync::Semaphore>,
    }

    impl BlockingChatBrain {
        fn new() -> (
            Arc<Self>,
            Arc<tokio::sync::Semaphore>,
            Arc<tokio::sync::Semaphore>,
        ) {
            let entered = Arc::new(tokio::sync::Semaphore::new(0));
            let release = Arc::new(tokio::sync::Semaphore::new(0));
            (
                Arc::new(Self {
                    entered: Arc::clone(&entered),
                    release: Arc::clone(&release),
                }),
                entered,
                release,
            )
        }
    }

    #[async_trait::async_trait]
    impl crate::ports::brain::Brain for BlockingChatBrain {
        async fn run_cycle(
            &self,
            req: crate::ports::types::CycleRequest,
            _host: &dyn crate::ports::brain::CycleHost,
        ) -> crate::Result<crate::ports::types::CycleResult> {
            let mut channel_responses = Vec::new();
            for event in &req.events {
                if let CompanyEvent::OperatorMessage { text, .. } = event {
                    self.entered.add_permits(1);
                    self.release.acquire().await.expect("released").forget();
                    channel_responses.push(crate::ports::types::OutboundMessage {
                        message_id: None,
                        task_id: None,
                        channel: "operator".into(),
                        agent: None,
                        text: format!("answered: {text}"),
                        steps: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    });
                }
            }
            Ok(crate::ports::types::CycleResult {
                channel_responses,
                new_traces: vec![crate::ports::types::CompressedTrace::now(
                    &req.cycle_id,
                    "blocking chat",
                )],
                ledger_deltas: Vec::new(),
                token_usage: crate::ports::types::TokenUsage::default(),
            })
        }
    }

    /// Polls `f` until it holds, or fails the test.
    async fn until(label: &str, mut f: impl AsyncFnMut() -> bool) {
        let ok = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !f().await {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok();
        assert!(ok, "{label}");
    }

    /// The operator messages `chat/history` currently shows for the main desk.
    async fn history_texts(app: &axum::Router) -> Vec<String> {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/chat/history")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        body.as_array()
            .expect("the history route answers with an array")
            .iter()
            .filter_map(|m| m["text"].as_str().map(str::to_string))
            .collect()
    }

    /// The company's turn rows, id → status.
    async fn turn_rows(runtime: &Arc<CompanyRuntime>) -> Vec<(String, String)> {
        let mut rows = runtime
            .runs()
            .list_runs(runtime.id(), &crate::ports::runs::RunFilter::default())
            .await
            .unwrap();
        rows.sort_by_key(|r| r.created_at_millis);
        rows.into_iter()
            .map(|r| (r.id, r.status.to_string()))
            .collect()
    }

    /// **Issue #983 — the direct regression for the observed empty history.**
    ///
    /// The operator's message used to be appended *inside* the per-company
    /// serial lock, so a message sent while another turn held that lock did not
    /// exist anywhere until the turn ahead of it finished. Reloading during a
    /// long turn showed an empty conversation: the operator could not see their
    /// own question, could not tell whether it had been received, and re-sent it.
    ///
    /// The blocking first turn is what makes this a real test. With a single
    /// turn the lock is free and the cycle appends immediately, so the bug is
    /// invisible — which is exactly why it survived. Two turns reproduce the
    /// serial train the field report saw with five.
    #[tokio::test]
    async fn a_queued_message_is_in_the_transcript_before_its_turn_runs() {
        let home_dir = home();
        let (brain, entered, release) = BlockingChatBrain::new();
        let state = build_state_with_brain(
            home_dir.path(),
            "running",
            AppConfig::default(),
            Some(brain),
        )
        .await;
        let app = router(state);

        // Turn one takes the lock and stops inside the brain.
        let first = tokio::spawn({
            let app = app.clone();
            async move { app.oneshot(chat_request("the first question")).await }
        });
        entered.acquire().await.expect("turn one entered").forget();

        // Turn two is accepted while turn one still owns the lock.
        let second = tokio::spawn({
            let app = app.clone();
            async move { app.oneshot(chat_request("the second question")).await }
        });

        until(
            "the queued message never reached the transcript",
            async || {
                history_texts(&app)
                    .await
                    .iter()
                    .any(|t| t == "the second question")
            },
        )
        .await;

        // …and it is there while its turn is provably not finished: no answer
        // has been journaled for either message.
        let texts = history_texts(&app).await;
        assert!(
            texts.contains(&"the first question".to_string()),
            "the running turn's own message is missing: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.starts_with("answered:")),
            "a turn finished before the assertion could run: {texts:?}"
        );

        release.add_permits(2);
        for turn in [first, second] {
            let response = turn.await.unwrap().unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    /// One POST journals **exactly one** `OperatorMessage`, and the response's
    /// `messageId` is that message's own sequence.
    ///
    /// The pin for the pre-journaled cycle path. The route now appends the
    /// message itself and hands the cycle the seq; a cycle that appended again
    /// would double every operator message in every transcript, and one that
    /// reported a seq of its own would hand the console an id that resolves to
    /// the wrong line — both silent, both only visible here.
    #[tokio::test]
    async fn one_post_journals_one_message_and_reports_its_seq() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let app = router(state);

        let response = app
            .clone()
            .oneshot(chat_request("just the one"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();

        let journaled: Vec<(EventSeq, String)> = runtime
            .events()
            .read_from(runtime.id(), EventSeq::new(0), 10_000)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|s| match s.event {
                CompanyEvent::OperatorMessage { text, .. } => Some((s.seq, text)),
                _ => None,
            })
            .collect();
        assert_eq!(
            journaled.len(),
            1,
            "one POST must journal one message, got {journaled:?}"
        );
        assert_eq!(journaled[0].1, "just the one");
        assert_eq!(
            body["messageId"].as_str(),
            Some(journaled[0].0.value().to_string().as_str()),
            "messageId must resolve to the message's own line"
        );
    }

    /// **Issue #983 — the direct regression for the serial train.**
    ///
    /// The per-company cycle lock is held for a whole turn with unbounded
    /// waiters, so five concurrent messages became a queue and the fifth
    /// inherited the whole queue's latency. Nothing recorded that, so an
    /// operator watching a slow company could not tell "my turn is queued" from
    /// "my turn is wedged" from "nothing was received".
    ///
    /// The two statuses are what makes the wait legible, which is why the row
    /// is created at accept and started only once the cycle holds the lock.
    /// Collapsing them — starting the row where it is created — would make both
    /// turns read `Running`, and this assertion is what stops that.
    #[tokio::test]
    async fn a_queued_turn_is_pending_while_the_running_one_holds_the_lock() {
        let home_dir = home();
        let (brain, entered, release) = BlockingChatBrain::new();
        let state = build_state_with_brain(
            home_dir.path(),
            "running",
            AppConfig::default(),
            Some(brain),
        )
        .await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let app = router(state);

        let first = tokio::spawn({
            let app = app.clone();
            async move { app.oneshot(chat_request("first")).await }
        });
        entered.acquire().await.expect("turn one entered").forget();
        let second = tokio::spawn({
            let app = app.clone();
            async move { app.oneshot(chat_request("second")).await }
        });

        // Compared as a sorted pair rather than in row order: two POSTs a
        // millisecond apart can tie on `created_at_millis`, and what is being
        // asserted is that the two turns hold *different* statuses at once, not
        // which row the store lists first.
        until(
            "the second turn never queued behind the first",
            async || {
                let mut statuses: Vec<String> = turn_rows(&runtime)
                    .await
                    .into_iter()
                    .map(|(_, status)| status)
                    .collect();
                statuses.sort();
                statuses == ["pending", "running"]
            },
        )
        .await;

        release.add_permits(2);
        for turn in [first, second] {
            assert_eq!(turn.await.unwrap().unwrap().status(), StatusCode::OK);
        }

        until("both turns must reach a terminal status", async || {
            turn_rows(&runtime)
                .await
                .iter()
                .all(|(_, status)| status == "succeeded")
        })
        .await;
        assert_eq!(turn_rows(&runtime).await.len(), 2, "one row per POST");
    }

    /// The same proof over a **real socket**, so the keystone rests on hyper's
    /// actual behaviour rather than on `oneshot` being a good model of it.
    ///
    /// This boots the production server — `axum::serve` over a bound
    /// `TcpListener` — writes the resolve by hand, and then hangs up mid-turn
    /// the way a proxy does when it gives up on a slow upstream. Hyper reads the
    /// peer's close while the handler is still pending and drops the request,
    /// which is precisely the cancellation #380's hosted tenant hit. A graceful
    /// `FIN` is enough; it does not take a reset.
    ///
    /// **The pause after the close is load-bearing.** Hyper does not learn the
    /// peer is gone the instant the client calls `close` — it learns when its
    /// connection task next polls the socket and reads EOF. Release the barrier
    /// before that happens and the turn finishes on its own merits, so the test
    /// passes whether or not the cycle is drop-safe and proves nothing. Measured
    /// while building this: without the pause, the pre-fix inline code passed
    /// this test; with it, the pre-fix code fails and the fix passes.
    #[tokio::test]
    async fn a_real_socket_close_does_not_cancel_the_follow_up_cycle() {
        use tokio::io::AsyncWriteExt;

        let home_dir = home();
        let c = stalled_company(home_dir.path()).await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = c.app.clone();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let body = serde_json::json!({ "verdict": "approve" }).to_string();
        let request = format!(
            "POST /api/v1/company/approvals/{} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Cookie: {}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            c.approval_id,
            crate::server::test_support::fixed_cookie("acme"),
            body.len(),
        );
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket.write_all(request.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();

        // The turn is under way — the verdict is journaled and the grant minted.
        // Now the client goes away without ever reading a response, and we wait
        // for hyper to actually notice (see the note above).
        c.entered.notified().await;
        socket.shutdown().await.unwrap();
        drop(socket);
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        assert!(
            c.runtime.grants.peek(&c.approval_id).is_some(),
            "the grant was minted before the socket closed"
        );

        c.release.notify_one();
        assert!(
            await_continuation(&c.runtime).await,
            "a real peer close cancelled the follow-up cycle: the grant is spent \
             and the agent never continued"
        );
        assert_eq!(c.runtime.grants.live_count(), 1);
        server.abort();
    }

    /// `detach` answers on the verdict, not on the turn (issue #383).
    ///
    /// This is the half that removes the *wait*, and with it #380's gateway
    /// timeout: the response is already in the operator's hands while the agent
    /// is demonstrably still mid-turn. The continuation then arrives on the
    /// event stream, where the console is already subscribed.
    #[tokio::test]
    async fn a_detached_resolve_answers_before_the_turn_finishes() {
        let home_dir = home();
        let c = stalled_company(home_dir.path()).await;

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            c.app.clone().oneshot(resolve_request(
                &c.approval_id,
                serde_json::json!({"verdict":"approve","detach":true}),
            )),
        )
        .await
        .expect("a detached resolve must not wait on the agent turn")
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "recorded": true, "alreadyResolved": false, "stillAwaiting": 0, "outcome": "settled" })
        );

        // The answer really did precede the work: the turn is only now under
        // way, and is still blocked.
        c.entered.notified().await;
        assert!(
            !continued(&c.runtime),
            "the turn had already finished, so this proved nothing about waiting"
        );
        assert!(
            c.runtime.grants.peek(&c.approval_id).is_some(),
            "the grant is minted before the response, not after the turn"
        );

        c.release.notify_one();
        assert!(
            await_continuation(&c.runtime).await,
            "a detached continuation must still land"
        );
        assert_eq!(c.runtime.grants.live_count(), 1);
    }

    /// **Issue #431.** A *detached* resolve — the verb the inline chat card
    /// uses — mints a standing grant when one was asked for, and that grant is
    /// visible on the one list both surfaces read.
    ///
    /// This pairing had no coverage before this test, which is why it looked
    /// covered: its neighbour above sends `detach` with no scope, so the grant
    /// it asserts is the single-use one, while every standing-grant test
    /// resolves *without* `detach`. Until #431 the console could not ask for
    /// this combination at all, so nothing exercised it; now the chat card can,
    /// it is the console's only way to mint a standing grant.
    ///
    /// It holds because `run_resolve` computes the scope and hands it to
    /// `resolve_approval_spawned` *before* it branches on `detach` — statement
    /// ordering inside one function, which a refactor could reverse without a
    /// single existing test going red. Hence asserting it rather than reading it.
    #[tokio::test]
    async fn a_detached_resolve_mints_a_standing_grant_and_lists_it() {
        let home_dir = home();
        let c = stalled_company_parking(home_dir.path(), grantable_tool_call()).await;

        // The same flag the inline card gates its scope control on: if this is
        // false the console offers no choice, and the rest of this is moot.
        assert!(
            c.runtime.pending_approvals()[0].broadly_grantable,
            "the fixture must be an approval a standing scope may be asked for"
        );

        let response = c
            .app
            .clone()
            .oneshot(resolve_request(
                &c.approval_id,
                serde_json::json!({
                    "verdict": "approve",
                    "detach": true,
                    "scope": "tool",
                    "expires_in_millis": 60 * 60 * 1000,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Minted by the time the detached answer is written, not merely promised
        // by it — the receipt says `recorded`, so a grant that appeared later
        // would make that a lie.
        assert_eq!(
            c.runtime.grants.standing_count(),
            1,
            "a detached resolve carrying a tool scope must mint a standing grant"
        );

        // And visible on the one list route both surfaces read, described the
        // same way — this is what "appears in the same list as one granted from
        // the page" means, there being only one list.
        let listed = c
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/grants")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let bytes = to_bytes(listed.into_body(), usize::MAX).await.unwrap();
        let rows: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["tool"], "file_write");
        assert_eq!(rows[0]["agent"], "ceo");

        // The continuation still lands, so the scope did not cost the detach.
        c.entered.notified().await;
        c.release.notify_one();
        assert!(
            await_continuation(&c.runtime).await,
            "a detached continuation must still land when a scope was granted"
        );
    }

    /// The default is unchanged: no `detach` key means the response still
    /// carries the follow-up cycle's messages, in the same `ChatResponse` shape
    /// every existing caller parses. Only the drop-safety is new.
    #[tokio::test]
    async fn the_default_resolve_still_answers_with_the_cycle_response() {
        let home_dir = home();
        let c = stalled_company(home_dir.path()).await;

        // Let the turn through the moment it starts.
        c.release.notify_one();
        let response = c
            .app
            .clone()
            .oneshot(resolve_request(
                &c.approval_id,
                serde_json::json!({"verdict":"approve"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value.get("responses").is_some_and(|r| r.is_array()),
            "the un-detached body is still a ChatResponse, got {value}"
        );
        assert!(
            continued(&c.runtime),
            "the un-detached resolve waited for the turn, as it always did"
        );
        assert_eq!(c.runtime.grants.live_count(), 1);
    }

    /// A second resolve of the same approval is a success, not a failure, and
    /// mints nothing (issue #243). `detach` reports that as `alreadyResolved`,
    /// which is what makes a retry after a timeout safe to *show* as a retry
    /// rather than as an error — the thing #380's operator had no way to know.
    #[tokio::test]
    async fn a_second_resolve_reports_already_resolved_and_mints_nothing() {
        let home_dir = home();
        let c = stalled_company(home_dir.path()).await;
        c.release.notify_one();

        let first = c
            .app
            .clone()
            .oneshot(resolve_request(
                &c.approval_id,
                serde_json::json!({"verdict":"approve","detach":true}),
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let bytes = to_bytes(first.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["alreadyResolved"], false);
        assert!(await_continuation(&c.runtime).await);

        let second = c
            .app
            .clone()
            .oneshot(resolve_request(
                &c.approval_id,
                serde_json::json!({"verdict":"approve","detach":true}),
            ))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        let bytes = to_bytes(second.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "recorded": true, "alreadyResolved": true, "stillAwaiting": 0, "outcome": "already_resolved" })
        );
        assert_eq!(
            c.runtime.grants.live_count(),
            1,
            "re-approving minted no second grant"
        );
    }

    /// **Issue #1449 on the wire.** A card past its deadline answers `expired`,
    /// on both response shapes, and journals no approval against the operator.
    ///
    /// The two shapes matter independently. The **detached** receipt is what the
    /// inline chat card reads; the **synchronous** `ChatResponse` is what the
    /// Approvals page reads — the surface the defect was reported on — and it
    /// never sees a receipt at all, so a discriminator that only rode on the
    /// receipt would have left the reproduced bug in place.
    #[tokio::test]
    async fn a_resolve_past_the_deadline_answers_expired_on_both_shapes() {
        let home_dir = home();
        // `approval_ttl_hours = 0`: anything parked is past its deadline the
        // instant it lands, which is the state an operator meets when they get
        // to a queue late.
        let expiring: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\napproval_ttl_hours = 0\n",
        )
        .unwrap();
        let state = build_state_with_brain_and_manifest(
            home_dir.path(),
            "running",
            AppConfig::default(),
            Some(Arc::new(StalledContinuationBrain {
                entered: Arc::new(tokio::sync::Notify::new()),
                release: Arc::new(tokio::sync::Notify::new()),
                parked: gated_tool_call(),
            })),
            expiring,
        )
        .await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let app = router(state);

        let response = app.clone().oneshot(chat_request("do it")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let approval_id = runtime.pending_approvals()[0].id.clone();

        // The detached shape.
        let detached = app
            .clone()
            .oneshot(resolve_request(
                &approval_id,
                serde_json::json!({"verdict":"approve","detach":true}),
            ))
            .await
            .unwrap();
        assert_eq!(detached.status(), StatusCode::OK);
        let bytes = to_bytes(detached.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["outcome"], "expired",
            "the host default-denied this; the receipt has to be able to say so, got {value}"
        );
        assert_eq!(
            runtime.grants.live_count(),
            0,
            "and it minted nothing, as it always did"
        );

        // The synchronous shape, on a second card of the same company.
        let response = app
            .clone()
            .oneshot(chat_request("do it again"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let second = runtime.pending_approvals()[0].id.clone();
        let sync = app
            .clone()
            .oneshot(resolve_request(
                &second,
                serde_json::json!({"verdict":"approve"}),
            ))
            .await
            .unwrap();
        assert_eq!(sync.status(), StatusCode::OK);
        let bytes = to_bytes(sync.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value.get("responses").is_some_and(|r| r.is_array()),
            "still a ChatResponse, got {value}"
        );
        assert_eq!(
            value["outcome"], "expired",
            "the Approvals page's own shape carries it too, got {value}"
        );
        assert_eq!(runtime.grants.live_count(), 0);
    }

    /// Both scope forms carry `detach` identically — the `/companies/{id}` route
    /// and the single-company alias are the same handler, and a console pointed
    /// at either must get the same contract.
    #[tokio::test]
    async fn detach_works_on_the_company_id_scope_too() {
        let home_dir = home();
        let c = stalled_company(home_dir.path()).await;
        c.release.notify_one();

        let response = c
            .app
            .clone()
            .oneshot(resolve_request_scoped(
                "/api/v1/companies/acme",
                &c.approval_id,
                serde_json::json!({"verdict":"approve","detach":true}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "recorded": true, "alreadyResolved": false, "stillAwaiting": 0, "outcome": "settled" })
        );
        assert!(await_continuation(&c.runtime).await);
        assert_eq!(c.runtime.grants.live_count(), 1);
    }

    #[tokio::test]
    async fn deny_with_amended_payload_is_400() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        // The contradiction is rejected before anything is settled, so `detach`
        // cannot turn it into a `200 { recorded: true }` over a decision that was
        // never taken (issue #383).
        for body in [
            r#"{"verdict":"deny","amended_payload":{"text":"edited"}}"#,
            r#"{"verdict":"deny","amended_payload":{"text":"edited"},"detach":true}"#,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/company/approvals/missing")
                        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "for {body}");
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["code"], "invalid_request");
        }
    }

    #[tokio::test]
    async fn a_session_is_required_and_sufficient() {
        // Replaces `operator_token_guards_routes`. That token could never be
        // set, so the test only ever proved the guard worked in a state no
        // deployment could reach; every real host served this route to anyone.
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = build_state(&home, "running", AppConfig::default()).await;

        // No credential at all: closed.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // A garbage bearer buys nothing either — there is no bearer path in
        // prosumer mode at all now.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // A signed-in human gets their own company.
        let cookie = crate::server::test_support::seed_admin(&state, "acme").await;
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ---- issue #66: the operator attention SSE feed ----

    use crate::ports::types::{EventSeq, StoredEvent};

    fn stored(event: CompanyEvent) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(7),
            company: CompanyId::new("acme"),
            event,
            at_millis: 1_700_000_000_000,
        }
    }

    #[test]
    fn projects_a_gap_with_structural_fields_only() {
        let value = super::project_stream_item_for_viewer(
            &EventStreamItem::Gap { missed: 44 },
            &std::collections::HashMap::new(),
            &Viewer::Operator,
            true,
        )
        .expect("a gap must reach the console");
        assert_eq!(
            value,
            serde_json::json!({ "type": "stream_gap", "missed": 44 })
        );
    }

    #[test]
    fn projects_agent_reply_with_chat_fields_and_steps() {
        use crate::ports::types::{TurnStep, TurnStepKind, TurnStepStatus};
        let v = super::project_event(&stored(CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            task_id: None,
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "shipped it".into(),
            steps: vec![TurnStep {
                kind: TurnStepKind::ToolCall,
                status: TurnStepStatus::Ok,
                label: "Reading messages".into(),
                detail: None,
                elapsed_ms: Some(12),
                ..TurnStep::default()
            }],
        }))
        .expect("agent_reply is an attention signal");
        assert_eq!(v["type"], "agent_reply");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["atMillis"], 1_700_000_000_000_u64);
        assert_eq!(v["chatId"], "General");
        assert_eq!(v["agentId"], "ceo");
        assert_eq!(v["text"], "shipped it");
        // The scrubbed timeline rides along so a live listener sees the steps.
        assert_eq!(v["steps"][0]["label"], "Reading messages");
        assert_eq!(v["steps"][0]["status"], "ok");
        // A channel reply names no thread, so the legacy frame is unchanged.
        assert!(v.get("parentId").is_none(), "unexpected parentId: {v}");
    }

    #[test]
    fn projects_agent_reply_with_viewer_mention_metadata() {
        use crate::ports::types::{Mention, MentionTarget};
        let stored = stored(CompanyEvent::AgentReply {
            mentions: vec![
                Mention {
                    target: MentionTarget::User { id: "u-1".into() },
                    text: "@Ada".into(),
                    offset: 0,
                    quiet: false,
                },
                Mention {
                    target: MentionTarget::Everyone,
                    text: "@everyone".into(),
                    offset: 5,
                    quiet: true,
                },
            ],
            mention_depth: 0,
            parent: None,
            task_id: None,
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "@Ada @everyone".into(),
            steps: Vec::new(),
        });
        let authors = std::collections::HashMap::from([(String::from("u-1"), String::from("Ada"))]);
        let value =
            super::project_event_for_viewer(&stored, &authors, &Viewer::User("u-1".into()), false)
                .expect("agent_reply is an attention signal");
        assert_eq!(
            value["mentions"],
            serde_json::json!([
                { "text": "@Ada", "offset": 0, "label": "Ada", "mine": true },
                { "text": "@everyone", "offset": 5, "label": "everyone", "mine": true, "quiet": true },
            ])
        );
    }

    /// Issue #1781 review, Codex P1: `history_for_desk` already hides an
    /// owner-fallback report from a non-admin on reload; this proves the live
    /// SSE projection agrees, rather than handing a non-admin console the full
    /// admin-only text the instant it lands.
    #[test]
    fn drops_owner_fallback_report_from_a_non_admin_viewer() {
        let event = stored(CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            task_id: None,
            chat_id: "operator".into(),
            agent_id: crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR.to_string(),
            text: "no admin has a mailbox".into(),
            steps: Vec::new(),
        });

        let non_admin = super::project_event_for_viewer(
            &event,
            &std::collections::HashMap::new(),
            &Viewer::User("member-1".into()),
            false,
        );
        assert!(
            non_admin.is_none(),
            "a non-admin viewer must not receive the admin-only report live: {non_admin:?}"
        );

        let admin = super::project_event_for_viewer(
            &event,
            &std::collections::HashMap::new(),
            &Viewer::User("admin-1".into()),
            true,
        )
        .expect("an admin viewer still receives the report live");
        assert_eq!(
            admin["agentId"],
            crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR
        );

        // The Operator viewer (issue #66's original, unrestricted principal)
        // must see it too — same as `project_event`'s `is_admin: true` default.
        let operator = super::project_event_for_viewer(
            &event,
            &std::collections::HashMap::new(),
            &Viewer::Operator,
            true,
        )
        .expect("the operator viewer still receives the report live");
        assert_eq!(operator["text"], "no admin has a mailbox");
    }

    #[test]
    fn projects_agent_reply_with_its_thread_parent() {
        let v = super::project_event(&stored(CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: Some(EventSeq::new(4)),
            task_id: None,
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "in the thread".into(),
            steps: Vec::new(),
        }))
        .expect("agent_reply is an attention signal");
        assert_eq!(v["parentId"], "4");
    }

    /// Issue #983: the accept frame carries the turn, the desk and the thread —
    /// and **nothing else**.
    ///
    /// The negative half is what this test is for. `TurnStarted` is the first
    /// frame on this stream that brackets an operator's own message, so it is
    /// the obvious place for somebody to "helpfully" add the text or the asker
    /// — which is exactly the payload the deny-by-default projection exists to
    /// keep off the wire, and which `OperatorMessage` is dropped to avoid.
    #[test]
    fn projects_turn_started_with_structural_keys_only() {
        use crate::ports::types::{Actor, ActorKind};
        let v = super::project_event(&stored(CompanyEvent::TurnStarted {
            turn_id: "turn-1".into(),
            chat_id: "General".into(),
            parent: Some(EventSeq::new(4)),
            by: Some(Actor {
                kind: ActorKind::User,
                id: "u-1".into(),
            }),
        }))
        .expect("an accepted turn is an attention signal");
        assert_eq!(v["type"], "turn_started");
        assert_eq!(v["turnId"], "turn-1");
        assert_eq!(v["chatId"], "General");
        assert_eq!(v["parentId"], "4");
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["type", "seq", "atMillis", "turnId", "chatId", "parentId"],
            "the accept frame grew a key: {v}"
        );

        // A turn answering the channel itself omits the thread rather than
        // sending null, so the console's check is a presence check.
        let v = super::project_event(&stored(CompanyEvent::TurnStarted {
            turn_id: "turn-2".into(),
            chat_id: "General".into(),
            parent: None,
            by: None,
        }))
        .expect("an accepted turn is an attention signal");
        assert!(v.get("parentId").is_none(), "unexpected parentId: {v}");
    }

    /// The settle frame says a turn is over and **not why**.
    ///
    /// `TurnFailed::error` is a reason in our own words that can name
    /// internals; the console learns the reason from the tenant-scoped run row.
    #[test]
    fn projects_turn_settled_without_the_failure_reason() {
        let v = super::project_event(&stored(CompanyEvent::TurnFailed {
            turn_id: "turn-1".into(),
            error: "connection to db-primary.internal refused".into(),
        }))
        .expect("a settled turn is an attention signal");
        assert_eq!(v["type"], "turn_settled");
        assert_eq!(v["turnId"], "turn-1");
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["type", "seq", "atMillis", "turnId"],
            "the settle frame grew a key: {v}"
        );
    }

    /// The operator's own message is **still** dropped (issue #983).
    ///
    /// Pinned because #983 added the two arms above right beside it, and the
    /// natural next step — "the console needs the message too, project it" —
    /// would put operator-authored free text onto this stream for the first
    /// time. It does not need it: the message is already in the POST's own
    /// response and in `chat/history`, which is the point of journaling it at
    /// accept time. If somebody later decides otherwise, they say so here.
    #[test]
    fn projects_nothing_for_the_operators_own_message() {
        assert!(
            super::project_event(&stored(CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                text: "the operator's own words".into(),
                by: None,
                chat: Some("General".into()),
                parent: None,
                deliverable: None,
                attachments: Vec::new(),
            }))
            .is_none(),
            "the operator's own message must not reach the console over SSE"
        );
    }

    /// A reaction is deliberately NOT on the attention stream (issue #364).
    ///
    /// Pinned rather than left to the deny-by-default fall-through, because the
    /// omission is a decision and not an oversight: the frame would have to
    /// carry the reacting person, and this stream has no per-viewer projection
    /// to turn an actor into a label. Reload-visibility is what the issue asks
    /// for. If someone later decides reactions should stream, this test is
    /// where they say so out loud.
    #[test]
    fn projects_nothing_for_a_reaction() {
        assert!(
            super::project_event(&stored(CompanyEvent::ReactionToggled {
                message_seq: EventSeq::new(4),
                emoji: "👍".into(),
                on: true,
                by: None,
            }))
            .is_none(),
            "a reaction must not reach the console over SSE"
        );
    }

    /// Issue #379: the park frame carries an id, a kind and the channel — and
    /// **nothing else**.
    ///
    /// The negative half is the load-bearing one. The effect's arguments are
    /// redacted in exactly one place (`pending_approvals`), and if this frame
    /// ever grew a `payload` key it would become a second surface that has to
    /// redact and one day will not. Asserting the absence is what makes that a
    /// build failure rather than a leak.
    #[test]
    fn projects_approval_parked_with_a_channel_and_no_payload() {
        let v = super::project_event(&stored(CompanyEvent::ApprovalParked {
            approval_id: ApprovalId::new("appr-1"),
            effect_kind: "payment.send".into(),
            thread: Some("desk-finance".into()),
        }))
        .expect("a parked approval is an attention signal");
        assert_eq!(v["type"], "approval_parked");
        assert_eq!(v["approvalId"], "appr-1");
        assert_eq!(v["kind"], "payment.send");
        assert_eq!(v["chatId"], "desk-finance");
        for forbidden in ["payload", "agent", "amountUsd", "effect", "args"] {
            assert!(
                v.get(forbidden).is_none(),
                "the park frame must stay thin — `{forbidden}` leaked: {v}",
            );
        }
        assert_eq!(
            v.as_object().unwrap().len(),
            6,
            "type, seq, atMillis, approvalId, kind, chatId — and nothing more: {v}",
        );
    }

    /// A park with no conversation behind it omits the channel entirely, so a
    /// console filtering by thread matches it nowhere and it stays on the
    /// Approvals page (#379).
    #[test]
    fn projects_approval_parked_without_a_channel_when_no_thread_produced_it() {
        let v = super::project_event(&stored(CompanyEvent::ApprovalParked {
            approval_id: ApprovalId::new("appr-cron"),
            effect_kind: "email.send".into(),
            thread: None,
        }))
        .expect("a parked approval is an attention signal");
        assert_eq!(v["type"], "approval_parked");
        assert!(
            v.get("chatId").is_none(),
            "a page-only approval must carry no channel: {v}",
        );
    }

    #[test]
    fn projects_agent_reply_omits_empty_steps() {
        let v = super::project_event(&stored(CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            task_id: None,
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "hi".into(),
            steps: Vec::new(),
        }))
        .expect("agent_reply is an attention signal");
        // A tool-less reply keeps the legacy wire shape — no `steps` key.
        assert!(v.get("steps").is_none());
        // …and an uncorrelated reply carries no `taskId` either, so the
        // pre-#185 wire shape is byte-for-byte what it was.
        assert!(v.get("taskId").is_none());
    }

    /// #185: the correlation key rides the SSE stream when — and only when — the
    /// event carries one. Both directions matter: its presence is what lets a
    /// live console route a frame to the right task, and its absence is what
    /// keeps the legacy shape intact for every ordinary chat reply.
    #[test]
    fn projects_task_id_only_when_the_event_is_correlated() {
        let reply = super::project_event(&stored(CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            task_id: Some("t-1".into()),
            chat_id: "t-1".into(),
            agent_id: "ceo".into(),
            text: "on it".into(),
            steps: Vec::new(),
        }))
        .expect("agent_reply is an attention signal");
        assert_eq!(reply["taskId"], serde_json::json!("t-1"));

        let failure = super::project_event(&stored(CompanyEvent::McpCallFailed {
            task_id: Some("t-1".into()),
            server: "gh".into(),
            tool: "issues".into(),
            status: "credential_required".into(),
            message: "needs auth".into(),
        }))
        .expect("mcp_call_failed is an attention signal");
        assert_eq!(failure["taskId"], serde_json::json!("t-1"));

        let uncorrelated = super::project_event(&stored(CompanyEvent::McpCallFailed {
            task_id: None,
            server: "gh".into(),
            tool: "issues".into(),
            status: "credential_required".into(),
            message: "needs auth".into(),
        }))
        .expect("mcp_call_failed is an attention signal");
        assert!(uncorrelated.get("taskId").is_none());
    }

    /// #185/#377: the dispatch terminal projects the structural fields, plus
    /// the conversation the card was raised from. `column` is the one that
    /// matters most — it is how a console tells a clean finish from a cancelled
    /// or failed run — and `chatId` is what says which channel it belongs in.
    #[test]
    fn projects_desk_task_completed_with_every_field() {
        let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".into(),
            desk: "engineer".into(),
            output: "shipped".into(),
            column: "in_review".into(),
            artifact_ids: Vec::new(),
            origin_chat_id: Some("engineering".into()),
            origin_parent: None,
        }))
        .expect("desk_task_completed is an attention signal");
        assert_eq!(v["type"], serde_json::json!("desk_task_completed"));
        assert_eq!(v["taskId"], serde_json::json!("t-1"));
        assert_eq!(v["desk"], serde_json::json!("engineer"));
        assert_eq!(v["column"], serde_json::json!("in_review"));
        assert_eq!(v["chatId"], serde_json::json!("engineering"));
        // The envelope's own keys still ride along — the console mints the
        // marker's identity from `seq` (issue #483's mechanism), so losing it
        // here would silently disable the reload dedupe.
        assert!(v.get("seq").is_some(), "{v}");
        assert!(v.get("atMillis").is_some(), "{v}");
    }

    /// Issue #377: the run's prose is **not** on this frame.
    ///
    /// The relay bubble (#151) already carries the agent's words into the same
    /// channel this marker lands in. Projecting `output` here as well would put
    /// one run's text into one conversation twice, and dropping it at the
    /// projection is what stops any later reader from reintroducing that.
    #[test]
    fn desk_task_completed_does_not_project_the_runs_prose() {
        let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".into(),
            desk: "engineer".into(),
            output: "the whole reply, verbatim".into(),
            column: "in_review".into(),
            artifact_ids: Vec::new(),
            origin_chat_id: Some("engineering".into()),
            origin_parent: None,
        }))
        .expect("desk_task_completed is an attention signal");
        assert!(v.get("output").is_none(), "{v}");
        assert!(
            !v.to_string().contains("the whole reply"),
            "the prose must not reach the wire under any key: {v}"
        );
    }

    /// Issue #377: a card nobody raised from a conversation omits `chatId`
    /// rather than sending null — so "board-created" is a presence check on the
    /// console, the same shape `approval_parked` uses for a page-only approval.
    #[test]
    fn desk_task_completed_omits_the_chat_id_for_a_board_created_card() {
        let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".into(),
            desk: "engineer".into(),
            output: "shipped".into(),
            column: "in_review".into(),
            artifact_ids: Vec::new(),
            origin_chat_id: None,
            origin_parent: None,
        }))
        .expect("desk_task_completed is an attention signal");
        assert!(v.get("chatId").is_none(), "{v}");
        assert_eq!(v["column"], serde_json::json!("in_review"));
    }

    /// Issue #1890 B: the thread inside the channel, on exactly the terms
    /// `chatId` rides on.
    ///
    /// Stringified, because the console keys threads by message id and a
    /// message id is a string there — `chat/history` renders the same root the
    /// same way, and the two must agree or the marker would render inline live
    /// and jump into a thread on reload.
    #[test]
    fn desk_task_completed_projects_the_thread_its_card_was_raised_in() {
        let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".into(),
            desk: "engineer".into(),
            output: "shipped".into(),
            column: "in_review".into(),
            artifact_ids: Vec::new(),
            origin_chat_id: Some("engineering".into()),
            origin_parent: Some(crate::ports::types::EventSeq::new(41)),
        }))
        .expect("desk_task_completed is an attention signal");
        assert_eq!(v["chatId"], serde_json::json!("engineering"));
        assert_eq!(v["parentId"], serde_json::json!("41"));
    }

    /// A card raised straight into a channel omits `parentId` rather than
    /// sending null — the same presence-check shape `chatId` takes, so the
    /// console reads "channel level" without a null check.
    #[test]
    fn desk_task_completed_omits_the_parent_for_a_channel_level_card() {
        let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".into(),
            desk: "engineer".into(),
            output: "shipped".into(),
            column: "in_review".into(),
            artifact_ids: Vec::new(),
            origin_chat_id: Some("engineering".into()),
            origin_parent: None,
        }))
        .expect("desk_task_completed is an attention signal");
        assert_eq!(v["chatId"], serde_json::json!("engineering"));
        assert!(v.get("parentId").is_none(), "{v}");
    }

    #[test]
    fn projects_task_dispatched() {
        let v = super::project_event(&stored(CompanyEvent::TaskDispatched {
            task_id: "t-42".into(),
            run_id: None,
        }))
        .expect("task_dispatched is an attention signal");
        assert_eq!(v["type"], "task_dispatched");
        assert_eq!(v["taskId"], "t-42");
    }

    /// Issue #464: an opened card reaches the console as its own frame. This is
    /// the half a unit test can prove — that the projection exists and carries
    /// the card; that the *board* redraws off it is a browser fact.
    #[test]
    fn projects_task_card_changed() {
        let v = super::project_event(&stored(CompanyEvent::TaskCardChanged {
            task_id: "t-77".into(),
            change: crate::runtime::CHANGE_OPENED.into(),
            column: Some("todo".into()),
        }))
        .expect("a board write is an attention signal");
        assert_eq!(v["type"], "task_card_changed");
        assert_eq!(v["taskId"], "t-77");
        assert_eq!(v["change"], "opened");
        assert_eq!(v["column"], "todo");
    }

    /// A removed card is projected without a column — the console's "is it
    /// gone?" check is a presence check, never a null one.
    #[test]
    fn projects_a_removed_card_without_a_column() {
        let v = super::project_event(&stored(CompanyEvent::TaskCardChanged {
            task_id: "t-77".into(),
            change: crate::runtime::CHANGE_REMOVED.into(),
            column: None,
        }))
        .expect("a board write is an attention signal");
        assert_eq!(v["change"], "removed");
        assert!(
            v.get("column").is_none(),
            "a removed card is in no column: {v}"
        );
    }

    /// Issue #327: the workspace's own frame. The stream is deny-by-default, so
    /// an event with no arm is silently unprojected — this is what proves the
    /// arm exists at all.
    ///
    /// Also pins what is **not** on the wire: no node name, no body. A note's
    /// text is operator- or agent-authored free text, and this frame's job is
    /// to say something moved, not to carry the tree.
    #[test]
    fn projects_workspace_changed_without_a_name_or_a_body() {
        let v = super::project_event(&stored(CompanyEvent::WorkspaceChanged {
            node_id: "n-9".into(),
            change: crate::runtime::CHANGE_UPDATED.into(),
        }))
        .expect("a workspace write must reach the console");
        assert_eq!(v["type"], "workspace_changed");
        assert_eq!(v["nodeId"], "n-9");
        assert_eq!(v["change"], "updated");
        assert!(v.get("name").is_none(), "no node name on the wire: {v}");
        assert!(v.get("content").is_none(), "no body on the wire: {v}");
    }

    #[test]
    fn projects_mcp_call_failed_with_scrubbed_message() {
        let v = super::project_event(&stored(CompanyEvent::McpCallFailed {
            task_id: None,
            server: "browserbase".into(),
            tool: "browse".into(),
            status: "tool_call_rejected".into(),
            message: "server rejected the call".into(),
        }))
        .expect("mcp_call_failed is an attention signal");
        assert_eq!(v["type"], "mcp_call_failed");
        assert_eq!(v["server"], "browserbase");
        assert_eq!(v["tool"], "browse");
        assert_eq!(v["status"], "tool_call_rejected");
        // The message is already scrubbed at the source; we forward exactly it.
        assert_eq!(v["message"], "server rejected the call");
    }

    #[test]
    fn projects_approval_resolved_without_the_actor() {
        let v = super::project_event(&stored(CompanyEvent::ApprovalResolved {
            approval_id: ApprovalId::new("ap-1"),
            verdict: Verdict::Approve,
            by: Actor {
                kind: ActorKind::User,
                // A user id must never reach the wire via the attention feed.
                id: "secret-user-id".into(),
            },
        }))
        .expect("approval_resolved is an attention signal");
        assert_eq!(v["type"], "approval_resolved");
        assert_eq!(v["approvalId"], "ap-1");
        assert_eq!(v["verdict"], "approve");
        // The actor is intentionally dropped — the projection carries no `by`,
        // and the serialized bytes never mention the user id.
        assert!(v.get("by").is_none(), "actor must not be projected");
        assert!(
            !v.to_string().contains("secret-user-id"),
            "user id leaked onto the wire"
        );
        // Issue #971: and a person's decision carries no `automatic` flag, so
        // the console's "an operator decided this" reading of its absence is
        // the correct one.
        assert!(
            v.get("automatic").is_none(),
            "a user's own decision is not automatic"
        );
    }

    /// **T6 (issue #971).** A host-side expiry says so, without saying who.
    ///
    /// The defect: an expiry appends `ApprovalResolved { Deny, System }`, this
    /// frame dropped the actor, and the console toasted "Approval denied" — so
    /// an operator was told they had declined a request they never saw. With a
    /// 24-hour deadline that stops being rare.
    ///
    /// The assertion above is **extended here, not replaced**: the new field is
    /// a bit derived from `by.kind`, and the no-actor / no-user-id property it
    /// is derived from has to keep holding, so it is re-asserted on this arm
    /// with a `System` actor whose id is equally secret.
    #[test]
    fn projects_a_host_side_expiry_as_automatic_without_the_actor() {
        let v = super::project_event(&stored(CompanyEvent::ApprovalResolved {
            approval_id: ApprovalId::new("ap-2"),
            verdict: Verdict::Deny,
            by: Actor {
                kind: ActorKind::System,
                // Even the system actor's id stays off the feed: the console
                // needs the *fact* that no person decided this, not the name of
                // the internal path that did.
                id: "expiry".into(),
            },
        }))
        .expect("approval_resolved is an attention signal");
        assert_eq!(v["type"], "approval_resolved");
        assert_eq!(v["approvalId"], "ap-2");
        assert_eq!(v["verdict"], "deny");
        assert_eq!(
            v["automatic"], true,
            "the console must be able to say the deadline passed rather than \
             attributing the deny to whoever is looking at it"
        );
        // The extended property, restated on this arm.
        assert!(v.get("by").is_none(), "actor must not be projected");
        assert!(
            !v.to_string().contains("expiry"),
            "the actor id must not reach the wire on this arm either"
        );
    }

    #[test]
    fn projects_task_steered_without_actor_or_instruction() {
        let v = super::project_event(&stored(CompanyEvent::TaskSteered {
            task_id: "t-9".into(),
            action: "redirect".into(),
            instruction: Some("focus on the API".into()),
            by: Some(Actor {
                kind: ActorKind::User,
                id: "secret-user-id".into(),
            }),
        }))
        .expect("task_steered is an attention signal");
        assert_eq!(v["type"], "task_steered");
        assert_eq!(v["taskId"], "t-9");
        assert_eq!(v["action"], "redirect");
        let wire = v.to_string();
        assert!(!wire.contains("secret-user-id"));
        assert!(!wire.contains("focus on the API"));
    }

    #[test]
    fn projects_workflow_created_without_the_actor() {
        let v = super::project_event(&stored(CompanyEvent::WorkflowCreated {
            workflow_id: "greeter".into(),
            name: "Greeter".into(),
            by: Some(Actor {
                kind: ActorKind::User,
                id: "secret-user-id".into(),
            }),
        }))
        .expect("workflow_created is an attention signal");
        assert_eq!(v["type"], "workflow_created");
        assert_eq!(v["workflowId"], "greeter");
        assert_eq!(v["name"], "Greeter");
        assert!(!v.to_string().contains("secret-user-id"));
    }

    /// Issue #259: the edit and delete signals project the same two fields and
    /// drop the actor, exactly like `workflow_created` above.
    #[test]
    fn projects_workflow_updated_and_deleted_without_the_actor() {
        let actor = || {
            Some(Actor {
                kind: ActorKind::User,
                id: "secret-user-id".into(),
            })
        };

        let v = super::project_event(&stored(CompanyEvent::WorkflowUpdated {
            workflow_id: "greeter".into(),
            name: "Greeter v2".into(),
            by: actor(),
        }))
        .expect("workflow_updated is an attention signal");
        assert_eq!(v["type"], "workflow_updated");
        assert_eq!(v["workflowId"], "greeter");
        assert_eq!(v["name"], "Greeter v2");
        assert!(!v.to_string().contains("secret-user-id"));

        let v = super::project_event(&stored(CompanyEvent::WorkflowDeleted {
            workflow_id: "greeter".into(),
            name: "Greeter".into(),
            by: actor(),
        }))
        .expect("workflow_deleted is an attention signal");
        assert_eq!(v["type"], "workflow_deleted");
        assert_eq!(v["workflowId"], "greeter");
        assert_eq!(v["name"], "Greeter");
        assert!(!v.to_string().contains("secret-user-id"));
    }

    #[test]
    fn projects_lifecycle_changed_without_the_actor() {
        let v = super::project_event(&stored(CompanyEvent::LifecycleChanged {
            from: "running".into(),
            to: "paused".into(),
            by: Actor {
                kind: ActorKind::Operator,
                id: "operator".into(),
            },
        }))
        .expect("lifecycle_changed is an attention signal");
        assert_eq!(v["type"], "lifecycle_changed");
        assert_eq!(v["from"], "running");
        assert_eq!(v["to"], "paused");
        assert!(v.get("by").is_none(), "actor must not be projected");
    }

    #[test]
    fn projects_payment_received() {
        let v = super::project_event(&stored(CompanyEvent::PaymentReceived {
            amount_usd: 25.0,
            memo: "invoice #1".into(),
        }))
        .expect("payment_received is an attention signal");
        assert_eq!(v["type"], "payment_received");
        assert_eq!(v["amountUsd"], 25.0);
        assert_eq!(v["memo"], "invoice #1");
    }

    // ---- issue #228: the workflow-run outcome projection ----

    fn delivery_row(
        node: &str,
        status: crate::ports::DeliveryStatus,
    ) -> crate::ports::DeliveryReport {
        crate::ports::DeliveryReport {
            node: node.into(),
            kind: "email".into(),
            target: Some("ada@example.com".into()),
            status,
            detail: "this recipient has never written to the company".into(),
            reason: crate::ports::DeliveryReason::RecipientNotEstablished,
        }
    }

    /// The live half of #228: a finished run reaches the console as it happens,
    /// carrying exactly the fields the run drawer already renders — so the
    /// console can toast an undelivered report instead of waiting for a reload.
    #[test]
    fn projects_workflow_run_finished_with_the_fields_the_drawer_renders() {
        let v = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".into(),
            scheduled: true,
            run_id: None,
            deliveries: vec![
                delivery_row("owner_summary", crate::ports::DeliveryStatus::Skipped),
                delivery_row("also_sent", crate::ports::DeliveryStatus::Sent),
            ],
            pending_approvals: vec!["review".into()],
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        }))
        .expect("workflow_run_finished is an attention signal");
        assert_eq!(v["type"], "workflow_run_finished");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["workflowId"], "digest");
        assert_eq!(v["scheduled"], true);
        assert_eq!(v["pendingApprovals"][0], "review");

        // Per-row node/kind/target/status/detail — the same shape the manual
        // run's HTTP response already ships to this console.
        let rows = v["deliveries"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["node"], "owner_summary");
        assert_eq!(rows[0]["kind"], "email");
        assert_eq!(rows[0]["status"], "skipped");
        assert_eq!(rows[0]["target"], "ada@example.com");
        assert!(
            rows[0]["detail"]
                .as_str()
                .unwrap()
                .contains("never written"),
            "the detail names the fix: {v}"
        );

        // A run that finished carries no `error` key, and `runId` — always
        // `None` today — is never a permanently-null key on the wire.
        assert!(v.get("error").is_none(), "{v}");
        assert!(v.get("runId").is_none(), "{v}");
    }

    /// The failure arm reaches the console too — it is the outcome that used to
    /// produce nothing but a host-stdout warning.
    #[test]
    fn projects_workflow_run_finished_with_the_failure_reason() {
        let v = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".into(),
            scheduled: true,
            run_id: None,
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: Some("no inference source for agent node `worker`".into()),
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        }))
        .expect("workflow_run_finished is an attention signal");
        assert_eq!(v["error"], "no inference source for agent node `worker`");
        assert_eq!(v["deliveries"].as_array().unwrap().len(), 0);
    }

    /// Issues #881 / #880: the blocked arm reaches the console live.
    ///
    /// Without it a console watching a run settle would be told it finished
    /// cleanly — no error, not cancelled, nothing delivered — and then the
    /// history it reloads a moment later would say the run blocked. The two
    /// surfaces read the same journal event, so they must project the same
    /// facts.
    #[test]
    fn projects_workflow_run_finished_with_its_blocked_nodes_and_parked_approvals() {
        let v = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".into(),
            scheduled: true,
            run_id: Some("run-b".into()),
            deliveries: Vec::new(),
            pending_approvals: vec!["spec".into()],
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
                node_id: "spec".into(),
                tools: vec!["publish_artifact".into()],
                approval_ids: vec!["appr-1".into()],
                unparkable: 0,
                stranded: 0,
            }],
            approvals: vec![crate::ports::WorkflowRunApprovalRow {
                node_id: Some("spec".into()),
                tool: Some("publish_artifact".into()),
                outcome: crate::ports::WorkflowApprovalOutcome::Parked,
                approval_id: Some("appr-1".into()),
            }],
        }))
        .expect("workflow_run_finished is an attention signal");
        assert_eq!(v["blockedNodes"][0]["nodeId"], "spec");
        assert_eq!(v["blockedNodes"][0]["tools"][0], "publish_artifact");
        assert_eq!(v["approvals"][0]["outcome"], "parked");
        assert!(
            v.get("error").is_none(),
            "a run waiting on a person did not fail: {v}"
        );

        // The presence-check discipline: a run that blocked on nobody sends
        // neither key, so an existing frame is byte-unchanged.
        let clean = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".into(),
            scheduled: true,
            run_id: None,
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        }))
        .expect("projects");
        assert!(clean.get("blockedNodes").is_none(), "{clean}");
        assert!(clean.get("approvals").is_none(), "{clean}");
    }

    /// Issue #371: the live per-node trail. Both arms project, both carry the
    /// run id that ties them to the run's settle-frame, and — the point — the
    /// node arm carries a status and a duration and nothing else.
    #[test]
    fn projects_the_per_node_progress_trail() {
        let started = super::project_event(&stored(CompanyEvent::WorkflowRunStarted {
            workflow_id: "digest".into(),
            run_id: "run-1".into(),
            scheduled: true,
            started_by: None,
            resume_semantic: None,
        }))
        .expect("workflow_run_started reaches the console");
        assert_eq!(started["type"], "workflow_run_started");
        assert_eq!(started["workflowId"], "digest");
        assert_eq!(started["runId"], "run-1");
        assert_eq!(started["scheduled"], true);
        assert!(
            started.get("startedBy").is_none(),
            "no sender projects no key: {started}"
        );

        // Issue #1862 prerequisite: when the journal carries a sender, the SSE
        // frame forwards it under `startedBy`.
        let started_with_sender = super::project_event(&stored(CompanyEvent::WorkflowRunStarted {
            workflow_id: "digest".into(),
            run_id: "run-1".into(),
            scheduled: false,
            started_by: Some(crate::ports::types::StartedBy::Agent("ceo".into())),
            resume_semantic: None,
        }))
        .expect("workflow_run_started reaches the console");
        assert_eq!(
            started_with_sender["startedBy"],
            serde_json::json!({"agent": "ceo"})
        );

        let node = super::project_event(&stored(CompanyEvent::WorkflowNodeFinished {
            workflow_id: "digest".into(),
            run_id: "run-1".into(),
            node_id: "ceo".into(),
            status: crate::ports::types::WorkflowNodeStatus::Error,
            elapsed_ms: 1234,
            diagnostics: Vec::new(),
            agent_run_id: None,
        }))
        .expect("workflow_node_finished reaches the console");
        assert_eq!(node["type"], "workflow_node_finished");
        assert_eq!(node["runId"], "run-1");
        assert_eq!(node["nodeId"], "ceo");
        assert_eq!(node["status"], "error");
        assert_eq!(node["elapsedMs"], 1234);

        // The scrubbing claim, stated as a test: an errored node projects a
        // status word and NOTHING that could carry the node's own words. The
        // event type has no field to hold them, so this can only regress by
        // widening the event — which is the point of keeping it closed.
        let mut keys: Vec<&str> = node
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "atMillis",
                "elapsedMs",
                "nodeId",
                "runId",
                "seq",
                "status",
                "type",
                "workflowId",
            ],
            "the node frame carries only structural fields: {node}"
        );
    }

    /// Issue #382: the per-node START bracket reaches the console too. Without
    /// its own arm it would fall to `project_event`'s `_ => return None` wildcard
    /// and be silently dropped — the exact trap this file has been bitten by
    /// three times — and the canvas would be back to guessing which node runs.
    /// It carries the ids and NOTHING else: no status or duration (the node has
    /// not run) and no input, so the frame is structural by construction.
    #[test]
    fn projects_the_per_node_started_bracket() {
        let node = super::project_event(&stored(CompanyEvent::WorkflowNodeStarted {
            workflow_id: "digest".into(),
            run_id: "run-1".into(),
            node_id: "ceo".into(),
        }))
        .expect("workflow_node_started reaches the console");
        assert_eq!(node["type"], "workflow_node_started");
        assert_eq!(node["workflowId"], "digest");
        assert_eq!(node["runId"], "run-1");
        assert_eq!(node["nodeId"], "ceo");

        // Structural-only: ids plus the envelope, and no status/duration/payload
        // slot the finish frame has. Regresses only by widening the event.
        let mut keys: Vec<&str> = node
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["atMillis", "nodeId", "runId", "seq", "type", "workflowId"],
            "the started frame carries only structural ids: {node}"
        );
    }

    /// Issue #371 also starts projecting the run id on the settle-frame — the
    /// key that lets the console clear the right canvas when two runs overlap.
    /// Still omitted for a pre-#371 row, so no permanently-null key appears.
    #[test]
    fn projects_the_run_id_on_a_finished_run_only_when_there_is_one() {
        let with_id = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".into(),
            scheduled: false,
            run_id: Some("run-9".into()),
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        }))
        .expect("projected");
        assert_eq!(with_id["runId"], "run-9");

        let legacy = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".into(),
            scheduled: false,
            run_id: None,
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        }))
        .expect("projected");
        assert!(legacy.get("runId").is_none(), "{legacy}");
    }

    #[test]
    fn drops_non_attention_and_raw_payload_events() {
        // The operator's own message, and every variant that carries a raw
        // third-party payload or is audit-only, is dropped so nothing unexpected
        // (or secret-bearing) ever reaches the console.
        //
        // This list is unchanged by #228: adding `workflow_run_finished` to the
        // projection widened the wire by exactly one listed variant, and this
        // test passing untouched is what proves the deny-by-default default
        // still drops everything it dropped before.
        let dropped = [
            CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            },
            CompanyEvent::WebhookReceived {
                channel: "email".into(),
                body: serde_json::json!({"authorization": "Bearer sk-secret"}),
            },
            CompanyEvent::A2aTaskReceived {
                from: "@peer".into(),
                task: serde_json::json!({"token": "sk-secret"}),
            },
            CompanyEvent::ScheduleFired {
                cron: "0 9 * * *".into(),
                prompt: "daily standup".into(),
            },
            CompanyEvent::FeedbackFiled {
                note: "too slow".into(),
            },
            CompanyEvent::MemoryFactDeleted {
                fact_id: "f-1".into(),
            },
        ];
        for event in dropped {
            assert!(
                super::project_event(&stored(event.clone())).is_none(),
                "event should be dropped from the SSE feed: {event:?}"
            );
        }
    }

    #[tokio::test]
    async fn events_route_streams_text_event_stream() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/events")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The SSE head is returned immediately; the body streams indefinitely, so
        // we assert the status + content-type without draining it.
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
    }

    #[tokio::test]
    async fn events_route_requires_a_session() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// The composer's own typing pings must not echo back to it — the bus has
    /// no per-listener addressing, so this filter is the only thing standing
    /// between "you typed" and a fresh "Alice is typing…" line under your own
    /// cursor.
    #[test]
    fn a_typing_frame_from_the_viewer_is_dropped_and_from_anybody_else_is_kept() {
        let mine = crate::turn_stream::LiveFrame::Typing(crate::turn_stream::TypingFrame {
            kind: "typing",
            user_id: "u1".into(),
            chat_id: "engineering".into(),
            parent_id: None,
            at_millis: 0,
        });
        assert!(super::is_own_typing_frame(&mine, Some("u1")));
        assert!(!super::is_own_typing_frame(&mine, Some("u2")));
        assert!(
            !super::is_own_typing_frame(&mine, None),
            "a machine credential with nobody behind it authors nothing to echo"
        );

        let presence = crate::turn_stream::LiveFrame::Presence(crate::turn_stream::PresenceFrame {
            kind: "presence",
            user_id: "u1".into(),
            status: "online",
            at_millis: 0,
        });
        assert!(
            !super::is_own_typing_frame(&presence, Some("u1")),
            "presence is left alone — only typing echoes"
        );
    }

    // -----------------------------------------------------------------------
    // Standing permissions (issue #374)
    // -----------------------------------------------------------------------

    /// Every contradictory or unbounded scope request is a 400, and none of them
    /// reaches the runtime.
    ///
    /// The approval id is deliberately one that does not exist: each of these
    /// must be refused at the edge, so the fact that resolving a missing
    /// approval would otherwise be a harmless no-op never gets a chance to mask
    /// a body that should not have been accepted.
    ///
    /// A deny may now ride the tool scope (issue #1458 — a standing refusal),
    /// so that pairing is asserted as *accepted* at the bottom rather than
    /// listed among the refusals.
    #[tokio::test]
    async fn a_contradictory_or_unbounded_scope_is_refused() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;

        let day: u64 = 24 * 60 * 60 * 1000;
        for (label, body) in [
            (
                "an argument edit and a standing grant contradict",
                format!(
                    r#"{{"verdict":"approve","scope":"tool","expires_in_millis":{day},"amended_payload":{{"to":"x"}}}}"#
                ),
            ),
            (
                "the deadline is mandatory",
                r#"{"verdict":"approve","scope":"tool"}"#.to_string(),
            ),
            (
                "zero is not a duration",
                r#"{"verdict":"approve","scope":"tool","expires_in_millis":0}"#.to_string(),
            ),
            (
                "past the seven-day cap is refused, never clamped",
                format!(
                    r#"{{"verdict":"approve","scope":"tool","expires_in_millis":{}}}"#,
                    MAX_STANDING_GRANT_MILLIS + 1
                ),
            ),
            (
                "a duration is meaningless on the once scope",
                format!(r#"{{"verdict":"approve","scope":"once","expires_in_millis":{day}}}"#),
            ),
        ] {
            let response = router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/company/approvals/appr-missing")
                        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{label}: must be refused at the edge"
            );
        }

        // An unrecognised scope is refused too, one layer earlier: `ResolveScope`
        // is a closed enum, so axum's JSON extractor rejects it as 422 before
        // any handler runs. The status differs from the checks above; what
        // matters is that it is never silently downgraded to `once`, which would
        // hand an operator a single call when they asked for a standing one.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/approvals/appr-missing")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"verdict":"approve","scope":"forever"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Exactly at the cap is fine — the boundary is inclusive.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/approvals/appr-missing")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"verdict":"approve","scope":"tool","expires_in_millis":{MAX_STANDING_GRANT_MILLIS}}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);

        // A deny riding the tool scope is no longer a contradiction: it mints a
        // standing refusal (issue #1458). Same edge validation as an approve —
        // duration mandatory, bounded, and the missing approval resolves as a
        // no-op — so it is accepted exactly where a matching approve would be.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/approvals/appr-missing")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"verdict":"deny","scope":"tool","expires_in_millis":{day}}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// The default body — no `scope` key at all — is accepted exactly as before.
    #[tokio::test]
    async fn an_omitted_scope_is_the_pre_374_request() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/approvals/appr-missing")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"verdict":"approve"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The grants list is empty on a fresh company, and revoking something that
    /// is not there is a 404 rather than a cheerful no-op.
    #[tokio::test]
    async fn the_grants_list_starts_empty_and_revoking_nothing_is_a_404() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;

        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/grants")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 0);

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/grants/nope")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// A standing grant is listed with the authenticated user's id, is revocable,
    /// and revoking is idempotent-by-404. Both scope forms answer.
    #[tokio::test]
    async fn a_standing_grant_is_listed_under_its_granter_and_can_be_revoked() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

        runtime
            .grants
            .grant_standing(crate::runtime::grants::StandingGrant {
                id: crate::runtime::grants::GrantId::new("g1"),
                agent: "ops".into(),
                workflow: None,
                tool: "workspace_write".into(),
                verdict: Verdict::Approve,
                granted_by: Actor {
                    kind: ActorKind::User,
                    id: "user-7".into(),
                },
                approval_id: ApprovalId::new("appr-1"),
                at_millis: 1_000,
                expires_at_millis: crate::ports::now_millis() + 60 * 60 * 1000,
                origin_thread: None,
                origin_parent: None,
                origin_task: None,
                scope: None,
            });

        // Both addressing forms list it.
        for uri in ["/api/v1/company/grants", "/api/v1/companies/acme/grants"] {
            let response = router(state.clone())
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value[0]["id"], "g1", "{uri}");
            assert_eq!(value[0]["tool"], "workspace_write");
            assert_eq!(value[0]["agent"], "ops");
            assert_eq!(
                value[0]["granted_by"]["id"], "user-7",
                "the list names who actually granted it"
            );
            assert!(
                value[0].get("payload").is_none() && value[0].get("args").is_none(),
                "a standing grant has no arguments, so the list opens no redaction surface"
            );
        }

        // Revoke, then it is gone and a second revoke is a 404.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/grants/g1")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(runtime.standing_grants().len(), 0);

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/company/grants/g1")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ---------------------------------------------------------------------
    // Issue #469 — a turn that parks several approvals.
    //
    // Every test above parks exactly one, which is the case that always
    // worked. The failure the operator hit needs more than one: four
    // `composio_execute` calls from a single turn, all approved, and then
    // silence. These drive that shape end to end over the real router.
    // ---------------------------------------------------------------------

    /// A brain that parks `parks` gated tool calls on one operator message and
    /// answers each `ApprovalResolved` it is told about.
    ///
    /// Deliberately shaped like `HarnessBrain`'s approval arm rather than like a
    /// convenient stub: it consults the live grant set and produces **no reply
    /// at all** when there is no grant left to redeem, because that silent
    /// no-op is exactly what the later of several follow-up cycles used to hit.
    struct MultiParkBrain {
        parks: usize,
        /// One entry per `ApprovalResolved` the brain was handed, across all
        /// cycles.
        decisions: Arc<std::sync::Mutex<Vec<String>>>,
        /// How many cycles ran in total (the first is the chat turn).
        cycles: Arc<std::sync::atomic::AtomicUsize>,
        /// The runtime, so the brain can reach the grant set the way the
        /// harness's re-dispatch does. Filled by the test after the build.
        rt: Arc<std::sync::OnceLock<Arc<CompanyRuntime>>>,
        /// Fail the continuation cycle, to exercise defect 4.
        fail_continuation: bool,
        /// Stamp a workflow run id onto every parked effect (issue #1092), so
        /// the park records the shape a workflow node's gated tool call has:
        /// explicitly unlinked from any card, and carrying a run.
        run_id: Option<String>,
        /// An `@mention` to append to every continuation reply. Exercises the
        /// durable half of a reply's mention: the re-issue's reply journaling
        /// must badge the person it names, same as the `/chat` path.
        continuation_mention: Option<String>,
    }

    #[async_trait::async_trait]
    impl crate::ports::brain::Brain for MultiParkBrain {
        async fn run_cycle(
            &self,
            req: crate::ports::types::CycleRequest,
            host: &dyn crate::ports::brain::CycleHost,
        ) -> crate::Result<crate::ports::types::CycleResult> {
            self.cycles
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut responses = Vec::new();
            for event in &req.events {
                match event {
                    CompanyEvent::OperatorMessage { .. } => {
                        // Deliberately uncatalogued slugs (issue #470): this
                        // brain exists to park a *number* of distinct calls and
                        // is indifferent to how any of them classify. They
                        // still land under the real action key, so each reaches
                        // the catalogue lookup and misses it, rather than
                        // carrying no action for the classifier to find.
                        for i in 0..self.parks {
                            let mut effect = gated_tool_call();
                            effect.payload =
                                crate::policy::test_support::composio_unclassified_args_numbered(i);
                            effect.run_id = self.run_id.clone();
                            host.park_effect(effect).await?;
                        }
                    }
                    CompanyEvent::ApprovalResolved { approval_id, .. } => {
                        if self.fail_continuation {
                            return Err(crate::error::OpenCompanyError::BackgroundTask(
                                "the continuation turn fell over".into(),
                            ));
                        }
                        self.decisions.lock().unwrap().push(approval_id.to_string());
                        let rt = self.rt.get().expect("the test wires the runtime");
                        let Some(grant) = rt.grants.peek(approval_id) else {
                            continue;
                        };
                        rt.grants.consume(&grant.agent, &grant.tool, &grant.args);
                        let mut text = format!("re-issued {approval_id}");
                        if let Some(mention) = &self.continuation_mention {
                            text.push(' ');
                            text.push_str(mention);
                        }
                        responses.push(crate::ports::types::OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: grant.agent.clone(),
                            agent: None,
                            text,
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                    }
                    _ => {}
                }
            }
            Ok(crate::ports::types::CycleResult {
                channel_responses: responses,
                new_traces: vec![crate::ports::types::CompressedTrace::now(
                    &req.cycle_id,
                    "multi-park cycle",
                )],
                ledger_deltas: Vec::new(),
                token_usage: crate::ports::types::TokenUsage::default(),
            })
        }
    }

    /// A company whose next turn parks four sign-offs.
    struct MultiParkCompany {
        app: axum::Router,
        runtime: Arc<CompanyRuntime>,
        approvals: Vec<ApprovalId>,
        decisions: Arc<std::sync::Mutex<Vec<String>>>,
        cycles: Arc<std::sync::atomic::AtomicUsize>,
    }

    async fn multi_park_company(
        home: &std::path::Path,
        parks: usize,
        chat: Option<&str>,
        fail_continuation: bool,
    ) -> MultiParkCompany {
        multi_park_company_run(home, parks, chat, fail_continuation, None, None).await
    }

    /// [`multi_park_company`], with the parked effects stamped as a workflow
    /// run (issue #1092).
    async fn multi_park_company_run(
        home: &std::path::Path,
        parks: usize,
        chat: Option<&str>,
        fail_continuation: bool,
        run_id: Option<&str>,
        continuation_mention: Option<&str>,
    ) -> MultiParkCompany {
        let decisions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cycles = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rt_slot: Arc<std::sync::OnceLock<Arc<CompanyRuntime>>> =
            Arc::new(std::sync::OnceLock::new());
        let state = build_state_with_brain(
            home,
            "running",
            AppConfig::default(),
            Some(Arc::new(MultiParkBrain {
                parks,
                decisions: decisions.clone(),
                cycles: cycles.clone(),
                rt: rt_slot.clone(),
                fail_continuation,
                run_id: run_id.map(str::to_string),
                continuation_mention: continuation_mention.map(str::to_string),
            })),
        )
        .await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let _ = rt_slot.set(runtime.clone());
        let app = router(state);

        let body = match chat {
            Some(chat) => serde_json::json!({ "text": "do it", "chat": chat }),
            None => serde_json::json!({ "text": "do it" }),
        };
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let approvals: Vec<_> = runtime
            .pending_approvals()
            .iter()
            .map(|a| a.id.clone())
            .collect();
        assert_eq!(approvals.len(), parks, "the turn parked {parks} sign-offs");

        MultiParkCompany {
            app,
            runtime,
            approvals,
            decisions,
            cycles,
        }
    }

    /// Every `AgentReply` in the log, as `chat_id|text` — what the console's
    /// event stream projects as an `agent_reply` frame and what a transcript
    /// reload rebuilds from. An empty list means the operator saw nothing.
    async fn agent_replies(runtime: &Arc<CompanyRuntime>) -> Vec<String> {
        use crate::ports::types::EventSeq;
        runtime
            .events()
            .read_from(runtime.id(), EventSeq::new(0), 10_000)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|s| match s.event {
                CompanyEvent::AgentReply { text, chat_id, .. } => Some(format!("{chat_id}|{text}")),
                _ => None,
            })
            .collect()
    }

    /// The authors of every journaled `AgentReply`, in order (issue #966).
    ///
    /// Separate from [`agent_replies`] because that one folds the author away.
    async fn agent_reply_authors(runtime: &Arc<CompanyRuntime>) -> Vec<String> {
        use crate::ports::types::EventSeq;
        runtime
            .events()
            .read_from(runtime.id(), EventSeq::new(0), 10_000)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|s| match s.event {
                CompanyEvent::AgentReply { agent_id, .. } => Some(agent_id),
                _ => None,
            })
            .collect()
    }

    fn approve_detached(id: &ApprovalId) -> Request<Body> {
        resolve_request(id, serde_json::json!({"verdict":"approve","detach":true}))
    }

    /// Waits for the follow-up work a detached resolve spawned to settle:
    /// first for the turn to unblock, then for its continuation to have
    /// journaled `expected_replies` `AgentReply` rows.
    ///
    /// # Condition, not clock (issue #1071)
    ///
    /// The second half used to be `sleep(400ms)` with a comment admitting what
    /// it was — "the continuation itself runs on a spawned task; let it finish".
    /// `ContinuationQueue::waiting()` drops to zero when the turn is
    /// **unblocked**, which is strictly earlier than when the continuation's
    /// replies are **written**, so the gap had to be covered by something. A
    /// fixed sleep covers it only on a machine fast enough that day: on a loaded
    /// CI runner the assertions read the event log first and came back short —
    /// `3` replies instead of `4`, or `[]` instead of `["ceo"]` — on branches
    /// with nothing to do with this code.
    ///
    /// Raising the sleep is the tempting fix and only moves the threshold. This
    /// waits for the thing the caller is about to assert, the same way the first
    /// half already waits for `waiting()`, under the same 10-second cap. A test
    /// that is going to check for N replies has no reason to proceed before N
    /// replies exist, and every reason not to.
    ///
    /// The count is the caller's because only the caller knows it. Passing a
    /// number smaller than the assertion would reintroduce the race quietly, so
    /// pass exactly what is asserted.
    async fn settle(runtime: &Arc<CompanyRuntime>, expected_replies: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while runtime.continuations.waiting() > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the turn never unblocked");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while agent_replies(runtime).await.len() < expected_replies {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the continuation never journaled {expected_replies} replies"));
    }

    /// **The keystone (issue #469).** A turn that parks four sign-offs, all
    /// approved, produces exactly ONE continuation — and an answer the operator
    /// can actually see.
    ///
    /// Before this, each resolve spawned its own follow-up cycle: four full
    /// re-runs of one turn, each told about one decision. They did not race —
    /// the per-company serial lock made them queue — but the later ones found
    /// the grants the earlier ones had redeemed and produced nothing at all.
    /// And none of it reached the operator either way, because the resolve
    /// route never journaled a continuation's replies, so no `agent_reply`
    /// frame was ever projected. Four approvals, four wasted turns, silence.
    #[tokio::test]
    async fn four_sign_offs_from_one_turn_produce_one_continuation() {
        let home_dir = home();
        let c = multi_park_company(home_dir.path(), 4, None, false).await;
        let before = c.cycles.load(std::sync::atomic::Ordering::SeqCst);

        let mut handles = Vec::new();
        for id in &c.approvals {
            let app = c.app.clone();
            let request = approve_detached(id);
            handles.push(tokio::spawn(
                async move { app.oneshot(request).await.unwrap() },
            ));
        }
        for handle in handles {
            assert_eq!(handle.await.unwrap().status(), StatusCode::OK);
        }
        settle(&c.runtime, 4).await;

        assert_eq!(
            c.cycles.load(std::sync::atomic::Ordering::SeqCst) - before,
            1,
            "one turn owes one continuation, not one per approval"
        );
        assert_eq!(
            c.decisions.lock().unwrap().len(),
            4,
            "the single continuation carries every decision, so the brain learns all four"
        );
        assert!(
            c.runtime.pending_approvals().is_empty(),
            "every sign-off was decided"
        );
        assert_eq!(
            agent_replies(&c.runtime).await.len(),
            4,
            "the continuation's answers must reach the event stream, or the operator \
             watches an approved action in silence"
        );
    }

    /// The two orders an operator can decide in must end in the same place.
    ///
    /// Approving four at once and approving them one at a time are the same
    /// request spread over a different span, and the gate is the last decision
    /// rather than a time window — so neither can produce more continuations
    /// than the other. A design that coalesced only what arrived together would
    /// pass the test above and still re-run the turn four times here.
    #[tokio::test]
    async fn deciding_one_at_a_time_ends_where_deciding_all_at_once_does() {
        let home_dir = home();
        let c = multi_park_company(home_dir.path(), 4, None, false).await;
        let before = c.cycles.load(std::sync::atomic::Ordering::SeqCst);

        for (i, id) in c.approvals.iter().enumerate() {
            let response = c.app.clone().oneshot(approve_detached(id)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let ran = c.cycles.load(std::sync::atomic::Ordering::SeqCst) - before;
            if i < 3 {
                assert_eq!(
                    ran,
                    0,
                    "the turn is still blocked on {} more sign-off(s); continuing now \
                     would re-park them",
                    3 - i
                );
            }
        }
        settle(&c.runtime, 4).await;

        assert_eq!(
            c.cycles.load(std::sync::atomic::Ordering::SeqCst) - before,
            1,
            "the last decision unblocks the turn, and it runs once"
        );
        assert_eq!(c.decisions.lock().unwrap().len(), 4);
        assert_eq!(agent_replies(&c.runtime).await.len(), 4);
    }

    /// The continuation answers in the conversation the sign-off was raised in.
    ///
    /// Not on the answering agent's own line: a desk channel's request and a
    /// direct message to that channel's lead are answered by the same teammate,
    /// so keying the reply on the agent delivers a channel's continuation into a
    /// private thread nobody is watching (issue #379's lesson, which the reply
    /// path had never learned — only the re-park had).
    #[tokio::test]
    async fn a_continuation_answers_in_the_thread_the_sign_off_was_raised_in() {
        let home_dir = home();
        let c = multi_park_company(home_dir.path(), 2, Some("sales"), false).await;

        for id in &c.approvals {
            let response = c.app.clone().oneshot(approve_detached(id)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        settle(&c.runtime, 2).await;

        let replies = agent_replies(&c.runtime).await;
        assert_eq!(replies.len(), 2, "both re-issues answered");
        assert!(
            replies.iter().all(|r| r.starts_with("sales|")),
            "the continuation must land in the channel the approval was raised in, got {replies:?}"
        );
    }

    /// **Issue #1092.** A workflow node's parked call, once approved, answers
    /// on its run — never as a direct message from the teammate that ran it.
    ///
    /// This is the wiring test for `continuation_fallback_chat_id`: the unit
    /// tests pin what the fallback *returns*, and this pins that
    /// `publish_continuation` actually uses it, through a real park, a real
    /// resolve and the journal the console reads back.
    ///
    /// The assertion is written against the agent id rather than only for the
    /// run id, because that is the regression: the leak put the re-issued
    /// turn's narration into `chat/history?desk=<teammate>`, where it rendered
    /// as an unprompted DM.
    #[tokio::test]
    async fn a_workflow_parks_continuation_answers_on_the_run_not_in_a_dm() {
        let home_dir = home();
        let c =
            multi_park_company_run(home_dir.path(), 1, None, false, Some("run-1092"), None).await;

        let response = c
            .app
            .clone()
            .oneshot(approve_detached(&c.approvals[0]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        settle(&c.runtime, 1).await;

        let replies = agent_replies(&c.runtime).await;
        assert_eq!(replies.len(), 1, "the re-issue answered once");
        let (chat_id, _) = replies[0].split_once('|').expect("chat_id|text");
        assert_eq!(
            chat_id, "run-1092",
            "a workflow park's continuation belongs to its run, got {replies:?}"
        );
        // The regression, stated as itself: before this fix the fallback was
        // the answering teammate's own id, so this is what the leaked row held.
        assert_ne!(
            chat_id, "ceo",
            "the re-issue must not be journaled as a DM from the teammate that ran it"
        );
    }

    /// **Codex P1 (pass 2).** A continuation's reply is journaled through
    /// `publish_continuation`, not the `/chat` turn — so a mention an agent
    /// types back in an approval follow-up used to render as a chip and
    /// nothing else: no badge, no durable row, exactly the person it is meant
    /// to reach (offline when the reply lands) getting neither.
    ///
    /// Both paths file through the same writer now; this pins that an `@user`
    /// in a continuation reply lands as a mention notification whose audience
    /// carries the person named, under the chat the continuation answered in.
    #[tokio::test]
    async fn a_continuation_reply_that_mentions_a_user_files_a_notification() {
        let home_dir = home();
        let c = multi_park_company_run(
            home_dir.path(),
            1,
            Some("sales"),
            false,
            None,
            Some("@harness-admin"),
        )
        .await;

        let users = c
            .runtime
            .users()
            .list_users(&CompanyId::new("acme"))
            .await
            .unwrap();
        let admin = users
            .iter()
            .find(|u| u.email == "harness-admin@example.test")
            .expect("the fixed admin is seeded");
        assert_eq!(
            admin.status,
            crate::ports::users::UserStatus::Active,
            "the admin must be an active, mentionable target"
        );

        let response = c
            .app
            .clone()
            .oneshot(approve_detached(&c.approvals[0]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        settle(&c.runtime, 1).await;
        // The notification is filed inside `publish_continuation`, after the
        // reply is journaled — `settle` only waits for the reply. A loaded CI
        // runner can reach this point before the notification append finishes,
        // so poll for it (issue #1665, Codex P1 regression).
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let notes = c
                    .runtime
                    .notifications()
                    .list(&CompanyId::new("acme"), &admin.id)
                    .await
                    .unwrap();
                if notes.iter().any(|n| n.notification.kind == "mention") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the mention notification never appeared");

        let notes = c
            .runtime
            .notifications()
            .list(&CompanyId::new("acme"), &admin.id)
            .await
            .unwrap();
        let mentions: Vec<_> = notes
            .into_iter()
            .filter(|n| n.notification.kind == "mention")
            .collect();
        assert_eq!(
            mentions.len(),
            1,
            "the continuation's mention must badge the person it names"
        );
        let note = &mentions[0].notification;
        assert_eq!(note.context.as_deref(), Some("sales"));
        assert_eq!(
            note.title, "Someone mentioned you in sales",
            "a continuation has no author, so the generic label is the honest one"
        );
        assert!(
            note.audience
                .as_ref()
                .is_some_and(|a| a.contains(&admin.id)),
            "the named user must be in the notification's audience"
        );
    }

    /// **Issue #379's routing, re-homed (issue #469).** The continuation
    /// resumes in the thread the sign-off was raised in — and in no other.
    ///
    /// Asserted in **both directions**, because either alone would pass on a
    /// mistake. A desk channel's request and a direct message to that channel's
    /// lead are answered by the same teammate, so a reply keyed on the agent
    /// lands a channel's continuation in a private line nobody is watching, and
    /// a reply keyed on the channel does the reverse.
    ///
    /// This used to be pinned inside the harness brain, against a hand-built
    /// grant. It moved here with the journaling: the thread comes off the park
    /// record now, so the strong version of the test is the one that lets a real
    /// turn stamp it and a real resolve read it back.
    #[tokio::test]
    async fn a_continuation_resumes_in_the_thread_it_was_raised_in_and_no_other() {
        async fn threads_for(chat: &str) -> Vec<String> {
            let home_dir = home();
            let c = multi_park_company(home_dir.path(), 1, Some(chat), false).await;
            let response = c
                .app
                .clone()
                .oneshot(approve_detached(&c.approvals[0]))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            settle(&c.runtime, 1).await;
            agent_replies(&c.runtime)
                .await
                .into_iter()
                .map(|r| r.split('|').next().unwrap().to_string())
                .collect()
        }

        // Raised in a desk channel: the continuation belongs to the channel.
        let desk = threads_for("desk-finance").await;
        assert_eq!(desk, vec!["desk-finance".to_string()]);
        assert_ne!(
            desk[0], "ceo",
            "a channel's approval must not resume in the desk lead's private DM"
        );

        // Raised in a direct message with that same lead: the mirror image.
        let dm = threads_for("ceo").await;
        assert_eq!(dm, vec!["ceo".to_string()]);
        assert_ne!(
            dm[0], "desk-finance",
            "a private line's approval must not resume in the desk channel"
        );
    }

    /// A single-approval turn is unchanged: it continues on that one decision,
    /// exactly as it did before the gate existed.
    #[tokio::test]
    async fn a_lone_sign_off_still_continues_on_its_own_decision() {
        let home_dir = home();
        let c = multi_park_company(home_dir.path(), 1, None, false).await;
        let before = c.cycles.load(std::sync::atomic::Ordering::SeqCst);

        let response = c
            .app
            .clone()
            .oneshot(approve_detached(&c.approvals[0]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        settle(&c.runtime, 1).await;

        assert_eq!(
            c.cycles.load(std::sync::atomic::Ordering::SeqCst) - before,
            1
        );
        assert_eq!(agent_replies(&c.runtime).await.len(), 1);
    }

    /// **Defect 4.** A continuation that fails tells the person waiting for it.
    ///
    /// The verdict and the grant are already durable at this point, so the
    /// failure is recoverable — but only for somebody who knows it happened.
    /// Before this the entire report was one `tracing::error!`: the agent was
    /// not told the outcome, and neither was the operator, who saw an approval
    /// they had granted produce nothing and had no way to tell a slow turn from
    /// a dead one.
    #[tokio::test]
    async fn a_failed_continuation_tells_the_operator() {
        let home_dir = home();
        let c = multi_park_company(home_dir.path(), 2, Some("sales"), true).await;

        for id in &c.approvals {
            let response = c.app.clone().oneshot(approve_detached(id)).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "the verdict is durable regardless of what the turn then does"
            );
        }
        settle(&c.runtime, 1).await;

        let replies = agent_replies(&c.runtime).await;
        assert_eq!(
            replies.len(),
            1,
            "the operator is told exactly once that the work did not resume, got {replies:?}"
        );
        assert!(
            replies[0].starts_with("sales|"),
            "and told in the thread they approved in, got {replies:?}"
        );
        assert!(
            replies[0].contains("approving again is safe"),
            "the notice has to say what to do about it, got {replies:?}"
        );
        // Issue #966, asserted on the journaled row rather than on the
        // constructor: this drives the real approve path, so it pins that
        // `announce_continuation_failure` *calls* the named notice. Asserting
        // the constructor alone leaves the call site free to go back to an
        // inline `AgentReply` authored by the operator channel — a correct
        // system row byte-identical to one the pre-#885 defect damaged.
        let authors = agent_reply_authors(&c.runtime).await;
        assert_eq!(
            authors,
            vec![crate::ports::SYSTEM_AUTHOR.to_string()],
            "the runtime authored this notice, so it must not be stored under its destination"
        );
    }

    /// Codex review finding: a stream that errors mid-read used to fall
    /// straight through to extraction on whatever partial bytes it had
    /// collected. This pins the fix directly against a synthetic stream,
    /// without needing a real workspace store behind it — a chunk, then an
    /// error, must discard everything read so far rather than handing back
    /// a truncated payload that looks complete.
    #[tokio::test]
    async fn drain_bounded_discards_everything_on_a_mid_stream_error() {
        use bytes::Bytes;
        use futures::stream;

        let items: Vec<crate::error::Result<Bytes>> = vec![
            Ok(Bytes::from_static(b"the first chunk read fine")),
            Err(crate::error::OpenCompanyError::Store(
                "transient read failure".to_string(),
            )),
        ];
        let synthetic: crate::ports::workspace::BlobStream = Box::pin(stream::iter(items));

        assert_eq!(drain_bounded(synthetic, 1_000_000).await, None);
    }

    /// The success twin: a stream with no error drains to its bytes, in
    /// order, across however many chunks it arrives in.
    #[tokio::test]
    async fn drain_bounded_concatenates_every_chunk_when_the_stream_never_errors() {
        use bytes::Bytes;
        use futures::stream;

        let items: Vec<crate::error::Result<Bytes>> = vec![
            Ok(Bytes::from_static(b"hello ")),
            Ok(Bytes::from_static(b"world")),
        ];
        let synthetic: crate::ports::workspace::BlobStream = Box::pin(stream::iter(items));

        assert_eq!(
            drain_bounded(synthetic, 1_000_000).await,
            Some(b"hello world".to_vec())
        );
    }

    /// A stream that never errors but exceeds the cap is also discarded, not
    /// truncated — the belt-and-braces the doc comment describes.
    #[tokio::test]
    async fn drain_bounded_discards_when_the_stream_exceeds_the_cap() {
        use bytes::Bytes;
        use futures::stream;

        let items: Vec<crate::error::Result<Bytes>> =
            vec![Ok(Bytes::from_static(b"way more than the cap allows"))];
        let synthetic: crate::ports::workspace::BlobStream = Box::pin(stream::iter(items));

        assert_eq!(drain_bounded(synthetic, 4).await, None);
    }

    /// A brain whose every reply names `@everyone` — the fixed shape for
    /// proving an agent reply's mentions file a notification, same as an
    /// operator message's already does.
    struct MentioningReplyBrain;

    #[async_trait::async_trait]
    impl crate::ports::brain::Brain for MentioningReplyBrain {
        async fn run_cycle(
            &self,
            req: crate::ports::types::CycleRequest,
            _host: &dyn crate::ports::brain::CycleHost,
        ) -> crate::Result<crate::ports::types::CycleResult> {
            let mut channel_responses = Vec::new();
            for event in &req.events {
                if matches!(event, CompanyEvent::OperatorMessage { .. }) {
                    channel_responses.push(crate::ports::types::OutboundMessage {
                        message_id: None,
                        task_id: None,
                        channel: "operator".into(),
                        agent: None,
                        text: "cc @everyone on this".into(),
                        steps: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    });
                }
            }
            Ok(crate::ports::types::CycleResult {
                channel_responses,
                new_traces: vec![crate::ports::types::CompressedTrace::now(
                    &req.cycle_id,
                    "mentioning reply",
                )],
                ledger_deltas: Vec::new(),
                token_usage: crate::ports::types::TokenUsage::default(),
            })
        }
    }

    /// **The Codex P1 finding:** `journal_chat_replies` resolved an agent
    /// reply's mentions and stored them on `CompanyEvent::AgentReply`, but never
    /// called `notify_mentions` — so an `@user` an agent typed *back* rendered
    /// as a chip and left the named person with no durable notification and no
    /// rail badge, unlike the operator's own message a few lines above it in
    /// the very same function. Missing it worst for exactly the person it is
    /// meant to reach: offline when the reply lands.
    #[tokio::test]
    async fn a_mention_in_an_agent_reply_notifies_the_person_it_names() {
        let home_dir = home();
        let state = build_state_with_brain(
            home_dir.path(),
            "running",
            AppConfig::default(),
            Some(Arc::new(MentioningReplyBrain)),
        )
        .await;
        // A second person for `@everyone` to reach — the sender is always
        // excluded from their own broadcast, so proving this needs somebody
        // else on the roster.
        crate::server::test_support::seed_fixed_member(&state, "acme").await;
        let app = router(state.clone());
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("company registered");

        let member_id = runtime
            .users()
            .list_users(&id)
            .await
            .unwrap()
            .into_iter()
            .find(|u| u.email == "harness-member@example.test")
            .expect("seeded member")
            .id;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"status?"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let notified = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let notes = runtime.notifications().list(&id, &member_id).await.unwrap();
                if !notes.is_empty() {
                    return notes;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the mentioned member was notified");

        assert_eq!(notified.len(), 1);
        assert_eq!(
            notified[0].notification.kind, "mention",
            "the reply's @everyone mention has to file the same kind of row an \
             operator message's does"
        );
    }

    /// **The Codex P1 finding:** the context a DM mention stores was decided by
    /// the human user directory, but a DM's thread id is a roster teammate's
    /// agent id — which no user record has — so a mention in a normal DM stored
    /// the bare id. The console's rail keys a DM by `dm:<teammate-id>` (and the
    /// console sends that bare id as the `chat` for a DM), so no rail row
    /// displayed the badge and opening the DM could neither match nor clear it.
    #[tokio::test]
    async fn a_mention_in_a_dm_stores_the_console_dm_channel_id() {
        let home_dir = home();
        let state = state_with_roster(home_dir.path()).await;
        // A second person for the broadcast to reach — the author is always
        // excluded from their own `@everyone`.
        crate::server::test_support::seed_fixed_member(&state, "acme").await;
        let app = router(state.clone());
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("company registered");

        let member_id = runtime
            .users()
            .list_users(&id)
            .await
            .unwrap()
            .into_iter()
            .find(|u| u.email == "harness-member@example.test")
            .expect("seeded member")
            .id;

        // A message addressed to the `designer` DM thread — the bare roster
        // teammate id, exactly what the console sends for a DM.
        let response = app
            .clone()
            .oneshot(chat_to("cc @everyone on this", Some("designer")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let notified = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let notes = runtime.notifications().list(&id, &member_id).await.unwrap();
                if !notes.is_empty() {
                    return notes;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the mentioned member was notified");

        // The offline echo brain answers the same text, so `@everyone` may land
        // twice — once for the operator's message, once for the echoed reply.
        // The count is incidental; the invariant is that *every* mention filed
        // out of this exchange is keyed to the console's `dm:designer` channel,
        // not the bare roster thread id.
        assert!(!notified.is_empty(), "the mentioned member was notified");
        let contexts: Vec<_> = notified
            .iter()
            .map(|n| n.notification.context.as_deref())
            .collect();
        assert!(
            contexts.iter().all(|c| *c == Some("dm:designer")),
            "every mention in a DM has to store the console's DM channel id, \
             not the bare roster thread id — got {contexts:?}"
        );
    }

    /// [`mention_context`] canonicalizes a **`dm:`-prefixed** noncanonical key
    /// too. An API client can address a DM with the console's channel shape but
    /// a noncanonical payload — `dm:BACKEND_ENGINEER` for the teammate whose id
    /// is `backend_engineer`. The routing resolves that case-insensitively, so
    /// the stored context has to carry the canonical agent id: filing the raw
    /// key under `dm:BACKEND_ENGINEER` badges a rail channel that does not
    /// exist, and opening the actual DM can never clear it. Pre-fix, the
    /// `dm:`-prefixed branch returned the key verbatim and bypassed
    /// `assignee::resolve` entirely.
    #[tokio::test]
    async fn mention_context_canonicalizes_prefixed_dm_keys() {
        let home_dir = home();
        let state = state_with_roster(home_dir.path()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("company registered");

        // A case-variant of the teammate's id, carrying the `dm:` prefix the
        // console mints.
        assert_eq!(
            runtime
                .mention_context(&id, &[], "dm:BACKEND_ENGINEER")
                .await,
            "dm:backend_engineer",
            "a `dm:`-prefixed noncanonical teammate key has to store dm:<agent-id>"
        );
        // The already-canonical shape stays unchanged — the resolution must
        // not move a key that was already right.
        assert_eq!(
            runtime
                .mention_context(&id, &[], "dm:backend_engineer")
                .await,
            "dm:backend_engineer",
            "a canonical dm:<teammate-id> key is kept as-is"
        );
        // A `dm:` key whose bare half names a desk (the desk-first ordering the
        // routing uses) files under the desk id, not a nonexistent `dm:<desk>`.
        assert_eq!(
            runtime.mention_context(&id, &[], "dm:Engineering").await,
            "engineering",
            "a `dm:` key that resolves to a desk has to store the desk id"
        );
    }

    /// A desk id that collides with a **human user id** still files under the
    /// desk. `assignee::resolve`'s desk-first ordering — the same one
    /// `responder_for` uses — outranks the user directory, and the directory
    /// must not get a say ahead of it. Pre-fix, a `users` pre-check ran before
    /// the resolution and returned `dm:<id>` for any bare key matching a human,
    /// so a mention aimed at a desk whose id happened to match a human id would
    /// badge a nonexistent DM channel and could never be cleared from the desk
    /// it was meant for.
    #[tokio::test]
    async fn mention_context_a_human_id_matching_a_desk_id_stays_a_desk() {
        let home_dir = home();
        let state = state_with_roster(home_dir.path()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("company registered");

        // A human whose id collides with the `engineering` desk's id. The human
        // directory must not win: the message is aimed at the desk.
        let human = crate::ports::users::UserRecord {
            id: "engineering".to_string(),
            email: "human@example.test".to_string(),
            display_name: None,
            avatar: None,
            role: crate::ports::users::UserRole::Member,
            status: crate::ports::users::UserStatus::Active,
            password_hash: None,
            must_change_password: false,
            created_at_millis: crate::ports::now_millis(),
            last_seen_at_millis: None,
            updated_at_millis: crate::ports::now_millis(),
        };

        assert_eq!(
            runtime
                .mention_context(&id, std::slice::from_ref(&human), "engineering")
                .await,
            "engineering",
            "a desk id that matches a human id files under the desk, not dm:<id>"
        );
        assert_eq!(
            runtime
                .mention_context(&id, std::slice::from_ref(&human), "dm:engineering")
                .await,
            "engineering",
            "the same collision through a dm:-prefixed key still files under the desk"
        );
        // A DM the human is actually a teammate of still badges as a DM.
        assert_eq!(
            runtime
                .mention_context(&id, &[human], "dm:backend_engineer")
                .await,
            "dm:backend_engineer",
            "a real DM channel is unaffected by the collision guard"
        );
    }

    /// [`mention_context`] resolves a `dm:`-prefixed key **as sent** before
    /// stripping the prefix, so a desk literally named `dm:engineering` keeps
    /// that id. Pre-fix, the unconditional strip resolved `engineering` instead
    /// and filed the badge under the wrong transcript — the exact claim
    /// [`assignee::dm_key`]'s contract warns about.
    #[tokio::test]
    async fn mention_context_a_desk_literally_named_dm_prefix_keeps_its_id() {
        let home_dir = home();
        let state = state_with_dm_prefixed_desk(home_dir.path()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("company registered");

        // The literal `dm:engineering` desk resolves as sent; stripping would
        // misroute to the plain `engineering` desk.
        assert_eq!(
            runtime.mention_context(&id, &[], "dm:engineering").await,
            "dm:engineering",
            "a desk literally named dm:<…> keeps its id — the raw key resolves first"
        );
        // The un-prefixed desk is untouched by the collision.
        assert_eq!(
            runtime.mention_context(&id, &[], "engineering").await,
            "engineering",
            "the un-prefixed desk still resolves to its own id"
        );
        // A genuine DM still re-keys onto the rail's DM channel.
        assert_eq!(
            runtime
                .mention_context(&id, &[], "dm:backend_engineer")
                .await,
            "dm:backend_engineer",
            "a real DM channel is unaffected by the literal dm: desk"
        );
    }

    /// [`mention_context`] stores the **canonical** id for a key typed in a
    /// noncanonical shape — a desk by its display name, a teammate by a
    /// case-variant of their id. `assignee::resolve` already returns canonical
    /// ids (issue #214); storing the raw key instead would file the badge under
    /// a channel id the rail never has, so it could neither render nor clear.
    #[tokio::test]
    async fn mention_context_stores_canonical_ids_for_noncanonical_keys() {
        let home_dir = home();
        let state = state_with_roster(home_dir.path()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("company registered");

        // A desk addressed by its display name files under the desk's id —
        // `"Engineering"` names the desk whose id is `engineering`.
        assert_eq!(
            runtime.mention_context(&id, &[], "Engineering").await,
            "engineering",
            "a desk named by its display name has to store the desk id, not the raw key"
        );
        // A teammate addressed by a case-variant of their id files under the
        // canonical agent id, re-keyed into the console's DM channel space.
        assert_eq!(
            runtime.mention_context(&id, &[], "BACKEND_ENGINEER").await,
            "dm:backend_engineer",
            "a teammate named by a noncanonical key has to store dm:<agent-id>"
        );
    }

    /// [`mention_context`] files a mention in the General desk — the default an
    /// unaddressed message lands in — under the console's canonical main-thread
    /// id even when this company has no desk named/id `General`. This fixture's
    /// only desk is `engineering`, so every general-chat spelling would
    /// otherwise fall through to the raw string and badge a rail row that does
    /// not exist (issue #1665 follow-up).
    #[tokio::test]
    async fn mention_context_maps_unresolvable_general_spellings_to_main() {
        let home_dir = home();
        let state = state_with_roster(home_dir.path()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("company registered");

        for general in ["General", "general", "main", ""] {
            assert_eq!(
                runtime.mention_context(&id, &[], general).await,
                crate::server::chat_history::MAIN_THREAD_ID,
                "a mention in the General desk ({general:?}) has to store the console's \
                 main-thread id, which the rail aliases onto its first rendered desk \
                 channel"
            );
        }
        // A desk that does resolve keeps its canonical id — the general-chat
        // mapping must not swallow a real desk.
        assert_eq!(
            runtime.mention_context(&id, &[], "Engineering").await,
            "engineering",
            "a real desk keeps its canonical id even when its name looks general"
        );
    }

    /// [`mention_context`] canonicalizes a **memberless** desk too. A desk that
    /// exists but has nobody seated on it is still a real desk with a real rail
    /// channel, so a key typed as its display name must file under its canonical
    /// id: `"Sales"` has to badge `#sales`, and opening `#sales` has to clear it.
    /// Pre-fix, `EmptyDesk` fell through the same wildcard as `Unknown` and
    /// stored the raw key — a channel id no desk renders, so the badge was
    /// invisible and could never clear.
    #[tokio::test]
    async fn mention_context_canonicalizes_a_memberless_desk() {
        let home_dir = home();
        let state = state_with_memberless_desk(home_dir.path()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("company registered");

        assert_eq!(
            runtime.mention_context(&id, &[], "Sales").await,
            "sales",
            "a memberless desk named by its display name has to store the desk id, \
             not the raw key — the rail's channel id is `sales`"
        );
        // The desk that does have a lead keeps behaving as before.
        assert_eq!(
            runtime.mention_context(&id, &[], "Engineering").await,
            "engineering",
            "a desk with a lead still stores its canonical id"
        );
    }

    /// Issue #1781 review (Codex P1): [`company_events`]'s periodic refresh
    /// must re-derive admin access from the live user record, not keep
    /// answering with whatever it was when the SSE stream opened. Proven
    /// directly against [`refreshed_is_admin`] — the seam that refresh loop
    /// calls on every tick — rather than the SSE handler itself, since the
    /// handler's own timing (a real `EventSource`, a 60s interval) is not
    /// what this bug is about.
    #[tokio::test]
    async fn refreshed_is_admin_reflects_a_mid_stream_demotion() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        let mut user = crate::ports::users::UserRecord {
            id: "u1".to_string(),
            email: "admin@acme.test".to_string(),
            display_name: None,
            avatar: None,
            role: crate::ports::users::UserRole::Admin,
            status: crate::ports::users::UserStatus::Active,
            password_hash: None,
            must_change_password: false,
            created_at_millis: crate::ports::now_millis(),
            last_seen_at_millis: None,
            updated_at_millis: crate::ports::now_millis(),
        };
        runtime
            .users()
            .upsert_user(runtime.id(), &user)
            .await
            .unwrap();
        let actor = Actor {
            kind: ActorKind::User,
            id: user.id.clone(),
        };

        assert!(
            refreshed_is_admin(&runtime, Some(&actor), false).await,
            "an active admin's record must resolve to admin, even starting from a stale `false`"
        );

        // The demotion itself: same shape `PATCH …/users/{id}` writes, and —
        // critically — it does not touch sessions, so a connection opened
        // before this write stays open exactly as it would in production.
        user.role = crate::ports::users::UserRole::Member;
        runtime
            .users()
            .upsert_user(runtime.id(), &user)
            .await
            .unwrap();

        assert!(
            !refreshed_is_admin(&runtime, Some(&actor), true).await,
            "a demoted user's live record must flip a stale `true` to `false` — this is \
             exactly the check `company_events` failed to make before this fix, leaking the \
             owner-fallback admin-only report to a demoted viewer for the rest of their stream"
        );

        // Suspension revokes admin the same way, even if role were untouched.
        user.role = crate::ports::users::UserRole::Admin;
        user.status = crate::ports::users::UserStatus::Suspended;
        runtime
            .users()
            .upsert_user(runtime.id(), &user)
            .await
            .unwrap();

        assert!(
            !refreshed_is_admin(&runtime, Some(&actor), true).await,
            "a suspended admin must not keep admin-only visibility either"
        );
    }

    /// Issue #1781 review, Codex P1 second follow-up: a human actor whose
    /// current role cannot be confirmed — `Ok(None)` because the user record
    /// has gone missing, folded in here with a genuine store error since both
    /// hit the same match arm — must resolve to `false`, not `previous`.
    ///
    /// `previous: true` here stands in for exactly the dangerous case: a
    /// cached "was admin" value from before whatever made this actor
    /// unconfirmable, revalidated at the one call site
    /// (`is_admin_for_item`) that gates the admin-only owner-fallback report
    /// on this result directly. Before this fix, an actor deleted out from
    /// under an open SSE stream — or a transient read failure landing at the
    /// exact moment a report needed gating — fell back to `previous` and kept
    /// leaking the report, silently, for as long as the failure (or the
    /// missing record) persisted.
    #[tokio::test]
    async fn refreshed_is_admin_fails_closed_when_the_user_record_cannot_be_found() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        // Never upserted — `get_user` answers `Ok(None)`, the "record has
        // gone missing" half of the case this proves.
        let actor = Actor {
            kind: ActorKind::User,
            id: "ghost".to_string(),
        };

        assert!(
            !refreshed_is_admin(&runtime, Some(&actor), true).await,
            "a human actor with no resolvable user record must read as not-admin \
             even when the cached value being revalidated was `true` — trusting \
             `previous` here is exactly the fail-open gap this fix closes"
        );
    }

    /// Issue #1781 review, Codex P1 follow-up: even with the periodic refresh
    /// the test above covers, `company_events` still only re-checked on its
    /// own `LABEL_REFRESH_EVERY` (60s) tick — a demotion landing right after
    /// one tick left an open SSE stream projecting an owner-fallback report
    /// under a stale cached `true` for up to another 60s. `is_admin_for_item`
    /// is the fix: it revalidates fresh for that one content class instead of
    /// trusting `cached`, no matter how long ago the last periodic tick was —
    /// proven here by feeding it a `cached: true` that is already wrong the
    /// instant this call happens, with no `sleep` at all.
    ///
    /// The second half is the other side of the same fix: an *ordinary* event
    /// must keep using `cached` untouched, or every SSE item would pay a
    /// store read regardless of content — the whole reason the fix is scoped
    /// to the owner-fallback content class rather than revalidating every
    /// item.
    #[tokio::test]
    async fn is_admin_for_item_revalidates_only_the_owner_fallback_report() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        let mut user = crate::ports::users::UserRecord {
            id: "u1".to_string(),
            email: "admin@acme.test".to_string(),
            display_name: None,
            avatar: None,
            role: crate::ports::users::UserRole::Admin,
            status: crate::ports::users::UserStatus::Active,
            password_hash: None,
            must_change_password: false,
            created_at_millis: crate::ports::now_millis(),
            last_seen_at_millis: None,
            updated_at_millis: crate::ports::now_millis(),
        };
        runtime
            .users()
            .upsert_user(runtime.id(), &user)
            .await
            .unwrap();
        let actor = Actor {
            kind: ActorKind::User,
            id: user.id.clone(),
        };

        // The demotion: no wait, no periodic tick — the very next item must
        // already see it for the gated content class.
        user.role = crate::ports::users::UserRole::Member;
        runtime
            .users()
            .upsert_user(runtime.id(), &user)
            .await
            .unwrap();

        let owner_fallback_item = EventStreamItem::Event(stored(CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            task_id: None,
            chat_id: "operator".into(),
            agent_id: crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR.to_string(),
            text: "no admin has a mailbox".into(),
            steps: Vec::new(),
        }));
        assert!(
            !super::is_admin_for_item(&owner_fallback_item, &runtime, Some(&actor), true).await,
            "an owner-fallback report must revalidate fresh and see the demotion \
             immediately — a stale cached `true` must never leak this content, \
             regardless of when the last periodic refresh ran"
        );

        let ordinary_item = EventStreamItem::Event(stored(CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            task_id: None,
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "ordinary reply".into(),
            steps: Vec::new(),
        }));
        assert!(
            super::is_admin_for_item(&ordinary_item, &runtime, Some(&actor), true).await,
            "an ordinary event must keep using the cached snapshot untouched — \
             revalidating every item, not just the gated content class, would \
             add a store read to the hot path for no reason"
        );
    }

    /// The machine principal has no user record to look up — `actor: None` —
    /// and [`ScopedCompany::is_admin`]'s own doc says it is unrestricted by
    /// construction, so the refresh must leave it alone rather than treating
    /// a missing actor as "look up nothing, therefore not admin".
    #[tokio::test]
    async fn refreshed_is_admin_leaves_the_machine_principal_unchanged() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        assert!(refreshed_is_admin(&runtime, None, true).await);
        assert!(!refreshed_is_admin(&runtime, None, false).await);
    }

    /// Two cards can be `in_review` on the same desk at once. Approving the
    /// pill the operator actually clicked must move that card and leave the
    /// other alone — resolving the desk's most-recently-updated card instead
    /// (Codex #3903031183) moves the wrong one whenever the older pill is
    /// clicked after a newer card has settled.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn review_card_settles_the_clicked_task_not_the_desks_latest() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        for (task_id, updated_at_millis) in [("t-old", 1u64), ("t-new", 2u64)] {
            runtime
                .tasks()
                .upsert(
                    runtime.id(),
                    &crate::ports::tasks::TaskRecord {
                        id: task_id.to_string(),
                        title: TaskTitle::authored("Ship it"),
                        note: None,
                        column: crate::ports::tasks::COLUMN_IN_REVIEW.to_string(),
                        priority: "medium".to_string(),
                        assignee: "ceo".to_string(),
                        updated_at_millis,
                        origin: crate::ports::TaskOrigin::new(Some("strategy".to_string()), None),
                        parent_task_id: None,
                        output: None,
                        plan: None,
                        planning_attempts: Vec::new(),
                        deliverable: crate::ports::tasks::TaskDeliverable::Once,
                        workflow_proposal: None,
                        origin_run_id: None,
                        origin_workflow_id: None,
                        origin_message_seq: None,
                        bounced: None,
                    },
                )
                .await
                .unwrap();
        }

        let scope = ScopedCompany {
            runtime: runtime.clone(),
            actor: None,
            may_read_contents: true,
            is_admin: true,
        };
        let receipt = review_card(
            scope,
            Json(ChatReviewRequest {
                chat_id: "strategy".to_string(),
                task_id: "t-old".to_string(),
                decision: "approve".to_string(),
                note: None,
            }),
        )
        .await
        .expect("the clicked card is settled")
        .0;
        assert_eq!(receipt.task_id, "t-old");
        assert_eq!(receipt.column, crate::ports::tasks::COLUMN_DONE);

        let cards = runtime.tasks().list(runtime.id()).await.unwrap();
        let old = cards.iter().find(|t| t.id == "t-old").unwrap();
        let new = cards.iter().find(|t| t.id == "t-new").unwrap();
        assert_eq!(
            old.column,
            crate::ports::tasks::COLUMN_DONE,
            "the clicked pill's card must settle"
        );
        assert_eq!(
            new.column,
            crate::ports::tasks::COLUMN_IN_REVIEW,
            "the desk's newer card must be untouched by a verdict on the older pill"
        );
    }

    /// A `task_id` naming a card outside the reviewed desk (or one that has
    /// already left `in_review`) must not resolve to some other card in the
    /// conversation — the request is rejected rather than silently falling
    /// back to "whatever is in review here".
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn review_card_rejects_a_task_id_not_in_review_on_this_desk() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        runtime
            .tasks()
            .upsert(
                runtime.id(),
                &crate::ports::tasks::TaskRecord {
                    id: "t-review".to_string(),
                    title: TaskTitle::authored("Ship it"),
                    note: None,
                    column: crate::ports::tasks::COLUMN_IN_REVIEW.to_string(),
                    priority: "medium".to_string(),
                    assignee: "ceo".to_string(),
                    updated_at_millis: 1,
                    origin: crate::ports::TaskOrigin::new(Some("strategy".to_string()), None),
                    parent_task_id: None,
                    output: None,
                    plan: None,
                    planning_attempts: Vec::new(),
                    deliverable: crate::ports::tasks::TaskDeliverable::Once,
                    workflow_proposal: None,
                    origin_run_id: None,
                    origin_workflow_id: None,
                    origin_message_seq: None,
                    bounced: None,
                },
            )
            .await
            .unwrap();

        let scope = ScopedCompany {
            runtime: runtime.clone(),
            actor: None,
            may_read_contents: true,
            is_admin: true,
        };
        let err = review_card(
            scope,
            Json(ChatReviewRequest {
                chat_id: "strategy".to_string(),
                task_id: "does-not-exist".to_string(),
                decision: "approve".to_string(),
                note: None,
            }),
        )
        .await
        .expect_err("an unknown task id must not fall back to the desk's own card");
        assert_eq!(
            axum::response::IntoResponse::into_response(err).status(),
            StatusCode::NOT_FOUND
        );
    }

    /// `apply_review_decision`'s `Revise` arm through the HTTP handler: the
    /// card re-enters `in_progress` with the operator's note appended, rather
    /// than settling to `done` the way `Approve` does.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn review_card_revise_re_enters_in_progress_with_the_note() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        runtime
            .tasks()
            .upsert(
                runtime.id(),
                &crate::ports::tasks::TaskRecord {
                    id: "t-1".to_string(),
                    title: TaskTitle::authored("Ship it"),
                    note: Some("[writer] first draft".to_string()),
                    column: crate::ports::tasks::COLUMN_IN_REVIEW.to_string(),
                    priority: "medium".to_string(),
                    assignee: "ceo".to_string(),
                    updated_at_millis: 1,
                    origin: crate::ports::TaskOrigin::new(Some("strategy".to_string()), None),
                    parent_task_id: None,
                    output: None,
                    plan: None,
                    planning_attempts: Vec::new(),
                    deliverable: crate::ports::tasks::TaskDeliverable::Once,
                    workflow_proposal: None,
                    origin_run_id: None,
                    origin_workflow_id: None,
                    origin_message_seq: None,
                    bounced: None,
                },
            )
            .await
            .unwrap();

        let scope = ScopedCompany {
            runtime: runtime.clone(),
            actor: None,
            may_read_contents: true,
            is_admin: true,
        };
        let receipt = review_card(
            scope,
            Json(ChatReviewRequest {
                chat_id: "strategy".to_string(),
                task_id: "t-1".to_string(),
                decision: "revise".to_string(),
                note: Some("tighten the intro".to_string()),
            }),
        )
        .await
        .expect("revise applies")
        .0;
        assert_eq!(receipt.task_id, "t-1");
        assert_eq!(receipt.column, crate::ports::tasks::COLUMN_IN_PROGRESS);

        let after = runtime
            .tasks()
            .list(runtime.id())
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == "t-1")
            .unwrap();
        let note = after.note.expect("note");
        assert!(note.contains("tighten the intro"), "{note}");
    }

    /// A thread reply intercepted as review feedback re-dispatches its card
    /// instead of answering with `responses` here. Codex #3903907771:
    /// `ChatView.send` reads an empty `responses` as "the turn produced
    /// nothing" and renders a synthetic "(no reply)" bubble underneath the
    /// operator's own feedback, even though the card was re-dispatched and
    /// will answer through its later relay. `reviewFeedbackApplied` is what
    /// tells the console this empty `responses` is expected.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn thread_reply_review_feedback_marks_the_response_not_empty_handed() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        runtime
            .tasks()
            .upsert(
                runtime.id(),
                &crate::ports::tasks::TaskRecord {
                    id: "t-1".to_string(),
                    title: TaskTitle::authored("Ship it"),
                    note: None,
                    column: crate::ports::tasks::COLUMN_IN_REVIEW.to_string(),
                    priority: "medium".to_string(),
                    assignee: "ceo".to_string(),
                    updated_at_millis: 1,
                    origin: crate::ports::TaskOrigin::new(Some("strategy".to_string()), None),
                    parent_task_id: None,
                    output: None,
                    plan: None,
                    planning_attempts: Vec::new(),
                    deliverable: crate::ports::tasks::TaskDeliverable::Once,
                    workflow_proposal: None,
                    origin_run_id: None,
                    origin_workflow_id: None,
                    origin_message_seq: None,
                    bounced: None,
                },
            )
            .await
            .unwrap();

        runtime
            .events()
            .append(
                runtime.id(),
                crate::ports::types::CompanyEvent::DeskTaskCompleted {
                    task_id: "t-1".to_string(),
                    desk: "ceo".to_string(),
                    output: "done".to_string(),
                    column: crate::ports::tasks::COLUMN_IN_REVIEW.to_string(),
                    artifact_ids: Vec::new(),
                    origin_chat_id: Some("strategy".to_string()),
                    origin_parent: None,
                },
            )
            .await
            .unwrap();
        let relay_seq = runtime
            .events()
            .append(
                runtime.id(),
                crate::ports::types::CompanyEvent::AgentReply {
                    chat_id: "strategy".to_string(),
                    agent_id: "ceo".to_string(),
                    text: "Here is the draft.".to_string(),
                    steps: Vec::new(),
                    task_id: None,
                    parent: None,
                    mentions: Vec::new(),
                    mention_depth: 0,
                },
            )
            .await
            .unwrap();

        let message = ChatMessage {
            text: "needs another pass".to_string(),
            chat: Some("strategy".to_string()),
            parent: Some(relay_seq.value().to_string()),
            deliverable: None,
            detach: false,
            mentions: None,
            attachments: Vec::new(),
        };

        let outcome = chat_and_emit(&state, &id, runtime.clone(), message, None)
            .await
            .expect("review feedback applies");
        let ChatOk::Settled(body) = outcome else {
            panic!("a synchronous review-feedback intercept must not detach");
        };
        assert!(body.responses.is_empty());
        assert_eq!(
            body.review_feedback_applied,
            Some(true),
            "an empty `responses` here must be marked expected, not read as \
             a silent turn"
        );

        let after = runtime
            .tasks()
            .list(runtime.id())
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == "t-1")
            .unwrap();
        assert_eq!(
            after.column,
            crate::ports::tasks::COLUMN_IN_PROGRESS,
            "the reply still re-dispatches the card"
        );
    }

    /// An unrecognized `decision` string rejects with `InvalidRequest` (400)
    /// rather than falling through to either verdict.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn review_card_rejects_an_unknown_decision() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        runtime
            .tasks()
            .upsert(
                runtime.id(),
                &crate::ports::tasks::TaskRecord {
                    id: "t-1".to_string(),
                    title: TaskTitle::authored("Ship it"),
                    note: None,
                    column: crate::ports::tasks::COLUMN_IN_REVIEW.to_string(),
                    priority: "medium".to_string(),
                    assignee: "ceo".to_string(),
                    updated_at_millis: 1,
                    origin: crate::ports::TaskOrigin::new(Some("strategy".to_string()), None),
                    parent_task_id: None,
                    output: None,
                    plan: None,
                    planning_attempts: Vec::new(),
                    deliverable: crate::ports::tasks::TaskDeliverable::Once,
                    workflow_proposal: None,
                    origin_run_id: None,
                    origin_workflow_id: None,
                    origin_message_seq: None,
                    bounced: None,
                },
            )
            .await
            .unwrap();

        let scope = ScopedCompany {
            runtime: runtime.clone(),
            actor: None,
            may_read_contents: true,
            is_admin: true,
        };
        let err = review_card(
            scope,
            Json(ChatReviewRequest {
                chat_id: "strategy".to_string(),
                task_id: "t-1".to_string(),
                decision: "yeet".to_string(),
                note: None,
            }),
        )
        .await
        .expect_err("an unknown decision string must not settle the card");
        assert_eq!(
            axum::response::IntoResponse::into_response(err).status(),
            StatusCode::BAD_REQUEST
        );
    }

    /// `POST {scope}/chat/review` end to end through the real router: proves
    /// the route is actually mounted by [`with_review_routes`] (not just that
    /// the handler function works when called directly) and that the wire
    /// body deserializes and settles the card via HTTP.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn chat_review_route_is_mounted_and_settles_via_http() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        runtime
            .tasks()
            .upsert(
                runtime.id(),
                &crate::ports::tasks::TaskRecord {
                    id: "t-1".to_string(),
                    title: TaskTitle::authored("Ship it"),
                    note: None,
                    column: crate::ports::tasks::COLUMN_IN_REVIEW.to_string(),
                    priority: "medium".to_string(),
                    assignee: "ceo".to_string(),
                    updated_at_millis: 1,
                    origin: crate::ports::TaskOrigin::new(Some("strategy".to_string()), None),
                    parent_task_id: None,
                    output: None,
                    plan: None,
                    planning_attempts: Vec::new(),
                    deliverable: crate::ports::tasks::TaskDeliverable::Once,
                    workflow_proposal: None,
                    origin_run_id: None,
                    origin_workflow_id: None,
                    origin_message_seq: None,
                    bounced: None,
                },
            )
            .await
            .unwrap();

        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat/review")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "chatId": "strategy",
                            "taskId": "t-1",
                            "decision": "approve",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["taskId"], "t-1");
        assert_eq!(value["column"], "done");
    }

    /// No card is `in_review` on the desk at all — as opposed to a `taskId`
    /// naming the wrong card, covered above — must also 404, through the same
    /// HTTP path the console calls.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn chat_review_route_404s_when_no_card_is_in_review() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home, "running").await;

        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat/review")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "chatId": "strategy",
                            "taskId": "t-1",
                            "decision": "approve",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "openhuman")]
    fn card_in_review(id: &str, chat_id: &str) -> crate::ports::tasks::TaskRecord {
        crate::ports::tasks::TaskRecord {
            id: id.to_string(),
            title: TaskTitle::authored("Ship it"),
            note: None,
            column: crate::ports::tasks::COLUMN_IN_REVIEW.to_string(),
            priority: "medium".to_string(),
            assignee: "ceo".to_string(),
            updated_at_millis: 1,
            origin: crate::ports::TaskOrigin::new(Some(chat_id.to_string()), None),
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            origin_message_seq: None,
            bounced: None,
        }
    }

    /// Two review verdicts racing the same `in_review` card (PR #1981 review
    /// finding, Codex P1) must not both resolve it before either applies —
    /// same `task_writes`-serialized load-modify-save shape
    /// `add_desk_member_serializes_against_the_company_write_lock` proves
    /// above, applied to `review_card`.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn review_card_serializes_against_the_task_writes_lock() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        runtime
            .tasks()
            .upsert(runtime.id(), &card_in_review("t-1", "strategy"))
            .await
            .unwrap();

        let guard = runtime.task_writes.lock().await;

        let runtime_for_task = runtime.clone();
        let mut task = tokio::spawn(async move {
            let scope = ScopedCompany {
                runtime: runtime_for_task,
                actor: None,
                may_read_contents: true,
                is_admin: true,
            };
            review_card(
                scope,
                Json(ChatReviewRequest {
                    chat_id: "strategy".to_string(),
                    task_id: "t-1".to_string(),
                    decision: "approve".to_string(),
                    note: None,
                }),
            )
            .await
        });

        let raced_ahead = tokio::time::timeout(Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "review_card resolved and applied a verdict while task_writes was \
             held elsewhere — it is not serializing against concurrent board \
             writers"
        );

        drop(guard);
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("review_card never resumed after task_writes was released")
            .expect("review_card task panicked");
        assert!(result.is_ok());
    }

    /// The revalidation half of the same finding: a review reply parked on
    /// `task_writes` while a second verdict already settled the card must see
    /// the now-current column once it resumes, not the stale `in_review`
    /// snapshot it would have clone from before it blocked — so it 404s
    /// instead of silently re-applying on top of the settled card.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn review_card_404s_when_the_card_left_review_while_the_reply_was_in_flight() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();

        runtime
            .tasks()
            .upsert(runtime.id(), &card_in_review("t-1", "strategy"))
            .await
            .unwrap();

        let guard = runtime.task_writes.lock().await;

        let runtime_for_task = runtime.clone();
        let mut task = tokio::spawn(async move {
            let scope = ScopedCompany {
                runtime: runtime_for_task,
                actor: None,
                may_read_contents: true,
                is_admin: true,
            };
            review_card(
                scope,
                Json(ChatReviewRequest {
                    chat_id: "strategy".to_string(),
                    task_id: "t-1".to_string(),
                    decision: "approve".to_string(),
                    note: None,
                }),
            )
            .await
        });
        let _ = tokio::time::timeout(Duration::from_millis(200), &mut task).await;

        let card = runtime
            .review_card_in_review("t-1", "strategy")
            .await
            .expect("task store lookup")
            .expect("card is still in_review before the lock is released");
        runtime
            .apply_review_decision(
                &card,
                crate::harness::built_in::lifecycle::ReviewDecision::Revise,
                Some("send it back"),
                None,
            )
            .await
            .unwrap();

        drop(guard);
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("review_card never resumed after task_writes was released")
            .expect("review_card task panicked");
        assert!(
            result.is_err(),
            "a review reply that had already resolved the card must not \
             silently re-apply its verdict once the card is no longer \
             in_review"
        );

        let after = runtime
            .tasks()
            .list(runtime.id())
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == "t-1")
            .unwrap();
        assert_eq!(after.column, crate::ports::tasks::COLUMN_IN_PROGRESS);
        let note = after.note.expect("note");
        assert!(note.contains("send it back"), "{note}");
    }
}
