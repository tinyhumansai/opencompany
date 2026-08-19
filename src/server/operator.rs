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
use tokio::task::JoinHandle;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::events::EventStreamItem;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, EventSeq, OutboundMessage, OverlayDesk,
    OverlayDeskMember, OverlayDeskOrder, StoredEvent, TurnStep, Verdict,
};
use crate::runtime::grants::{GrantId, GrantScope, MAX_STANDING_GRANT_MILLIS};
use crate::runtime::types::{ApprovalSummary, CompanyStatus, CycleReport};
use crate::server::chat_history::{
    CHAT_HISTORY_PAGE_LIMIT, MessageView, ReactionView, Viewer, channel_attributed_replies,
    history_for_desk,
};
use crate::server::error::ApiError;
use crate::server::graphql::auth::GqlAuth;
use crate::server::ops::language::{self, DEFAULT_DESK};
use crate::server::ops::{ScopedCompany, scoped};
use crate::server::platform_auth::{CompanyAuth, authorize_address, refuse_until_password_changed};
use crate::server::provision::{emit_cycle_webhooks, emit_feedback_webhook};

/// Builds the operator route fragment, merged into the main router.
pub fn router() -> Router<AppState> {
    Router::new()
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
        // The company → operator attention feed (issue #66): a live SSE stream of
        // the attention-worthy events already on the company's event log, under
        // both scope forms.
        .merge(scoped("/events", get(company_events)))
        // Standing permissions (issue #374): what the operator has opened up,
        // and how to take it back. Registered under both scope forms.
        .merge(scoped("/grants", get(list_grants)))
        .merge(scoped("/grants/{gid}", delete(revoke_grant)))
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
async fn list_desks(scope: ScopedCompany) -> Result<Json<Vec<DeskDto>>, Response> {
    let record = scope
        .runtime
        .store()
        .load(scope.id())
        .await
        .map_err(|e| ApiError(e).into_response())?;
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
                    overlay_created: false,
                }
            });
            let overlay_desks = record.overlay_desks.iter().map(|desk| {
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
                    overlay_created: true,
                }
            });
            manifest_desks.chain(overlay_desks).collect()
        })
        .unwrap_or_default();
    Ok(Json(desks))
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
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;
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
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;
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
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;
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
    /// The desk's founding member ids, in order (the first becomes the lead).
    /// Each must resolve to a roster teammate. Optional — a desk can start empty
    /// and gain members through the desk-member overlay.
    #[serde(default)]
    members: Vec<String>,
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

    let description = body
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    let desk = OverlayDesk {
        id: id.clone(),
        name: name.clone(),
        description: description.clone(),
        members: members.clone(),
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
    let mut record = scope
        .runtime
        .store()
        .load(scope.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(scope.id().to_string()))?;

    // A manifest desk belongs to the blueprint — never deletable at runtime.
    if record.manifest.group_chats.iter().any(|c| c.id == desk_id) {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::MANIFEST_DESK_DELETE.to_string(),
        )));
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
struct SseStreamGuard(CompanyId);

impl Drop for SseStreamGuard {
    fn drop(&mut self) {
        tracing::debug!(company = %self.0, "operator SSE stream closed");
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
    let guard = SseStreamGuard(company.clone());
    let durable = scope
        .runtime
        .events()
        .subscribe(&company)
        .filter_map(move |item| {
            // Keep the teardown guard alive for the life of the stream.
            let _ = &guard;
            let event = project_stream_item(&item)
                .map(|value| Ok(Event::default().data(value.to_string())));
            std::future::ready(event)
        });
    // Merge the transient live turn-progress bus (tool_call/tool_result frames a
    // turn emits while it runs — see [`crate::turn_stream`]) onto the same feed.
    // These are ephemeral and never journaled; the console switches on `type`
    // just like the durable projections. On a company with no active turn this
    // stream is simply quiet.
    let live = crate::turn_stream::subscribe(&company).map(|frame| {
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

/// Projects a live subscription item into the operator stream's safe wire
/// shape. A gap is an unpersisted control frame, deliberately structural-only.
fn project_stream_item(item: &EventStreamItem) -> Option<serde_json::Value> {
    match item {
        EventStreamItem::Event(stored) => project_event(stored),
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
fn project_event(stored: &StoredEvent) -> Option<serde_json::Value> {
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
        } => {
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
            ..
        } => {
            let mut o = envelope("desk_task_completed");
            o["taskId"] = json!(task_id);
            o["desk"] = json!(desk);
            o["column"] = json!(column);
            if let Some(chat_id) = origin_chat_id {
                o["chatId"] = json!(chat_id);
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
        } => {
            let mut o = envelope("workflow_run_started");
            o["workflowId"] = json!(workflow_id);
            o["runId"] = json!(run_id);
            o["scheduled"] = json!(scheduled);
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
        } => {
            let mut o = envelope("workflow_node_finished");
            o["workflowId"] = json!(workflow_id);
            o["runId"] = json!(run_id);
            o["nodeId"] = json!(node_id);
            o["status"] = json!(status);
            o["elapsedMs"] = json!(elapsed_ms);
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
) -> Result<Json<CompanyStatus>, Response> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp);
    }
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    runtime
        .status()
        .await
        .map(Json)
        .map_err(|e| ApiError(e).into_response())
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
    /// Whether an actionable request in this message opens a one-off card or a
    /// workflow card (issue #580). The operator chooses explicitly (decision
    /// D2a); absent means `once`, so an ordinary chat request is unchanged. Only
    /// consulted when the message actually carries a task intent — a greeting or
    /// a question opens no card regardless.
    #[serde(default)]
    deliverable: Option<crate::ports::tasks::TaskDeliverable>,
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
        !confined && message.deliverable == Some(crate::ports::tasks::TaskDeliverable::Workflow);
    if let Some(title) = (!confined)
        .then(|| crate::company::task_intent::triage_message(&message.text))
        .and_then(|triage| match triage {
            crate::company::task_intent::MessageTriage::Track(title) => Some(title),
            crate::company::task_intent::MessageTriage::Answer
            | crate::company::task_intent::MessageTriage::Chatter => None,
        })
        .or_else(|| {
            workflow_requested.then(|| crate::company::task_intent::to_title(message.text.trim()))
        })
        .filter(|title| !title.trim().is_empty())
    {
        // Keep the full message as the note only when the title was shortened
        // from it, so a one-line ask doesn't duplicate itself.
        let note =
            (title.trim_end_matches('…') != message.text.trim()).then(|| message.text.clone());
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
            // Issue #982: the thread this card was opened from, so the settle
            // marker lands back in the conversation that asked for the work
            // rather than only on the board. This is the field #151 added for
            // exactly that (`relay_reply` answers in the origin thread), and the
            // console already renders a marker in a DM channel — nothing there
            // changes. `None` for an unaddressed message, which is every card
            // this site opened before and therefore no change for one.
            origin_chat_id: message.chat.clone(),
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
            deliverable: message.deliverable.unwrap_or_default(),
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
            origin_workflow_id: None,
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
    };
    let message_seq = runtime
        .events()
        .append(id, message_event.clone())
        .await
        .map_err(ApiError)?;

    let turn_id = crate::ports::generate_id();
    let turn_id = match runtime
        .runs()
        .create_run(
            id,
            crate::ports::runs::NewRun::for_chat(turn_id.clone(), desk, desk),
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
) -> Result<Json<ChatResponse>, ApiError> {
    // The default desk for an unaddressed message.
    let desk = message
        .chat
        .clone()
        .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string());
    // Issue #364: a thread reply names its parent by id. Rejected here rather
    // than dropped, so a console sending a malformed parent learns that its
    // reply would have landed in the channel instead of quietly finding it
    // there later.
    let parent = match message.parent.as_deref() {
        Some(raw) => Some(parse_message_id(raw)?),
        None => None,
    };
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
    let (report, feedback_note) = join_chat_turn(spawn_chat_turn(ChatTurn {
        runtime,
        company: id.clone(),
        desk,
        message,
        by,
        parent,
        accepted,
    }))
    .await?;
    emit_cycle_webhooks(state, id, &report).await;
    if let Some(note) = feedback_note {
        emit_feedback_webhook(state, id, &note).await;
    }
    Ok(Json(ChatResponse {
        // The operator's own message is the cycle's single input event, so its
        // sequence is the first the cycle journaled (issue #364).
        message_id: report.input_seqs.first().map(|seq| seq.value().to_string()),
        responses: report.responses,
        // A chat turn is nobody's sign-off, so this stays absent here.
        still_awaiting: None,
    }))
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
                settle_chat_turn(&runtime, &company, turn_id.as_deref(), Some(&err)).await;
                return Err(err);
            }
        };
        journal_chat_replies(&runtime, &company, &desk, parent, &mut report).await;
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
async fn journal_chat_replies(
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
        let journaled = runtime
            .events()
            .append(
                id,
                CompanyEvent::AgentReply {
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
            Ok(seq) => response.message_id = Some(seq.value().to_string()),
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
) -> Result<Option<Actor>, Response> {
    use crate::server::graphql::auth::{GqlAuth, resolve_principal};

    // `peer` is threaded from every one of this function's callers, all the
    // way from their own handler's `MaybePeer` extractor, so `local_owner`'s
    // loopback-peer gate applies on this surface exactly as it does through
    // `CompanyAuth` and the GraphQL handler.
    let auth = resolve_principal(headers, state, Some(company), peer)
        .await
        .map_err(|_| unauthorized_response())?;
    if let Some(resp) = authorize_address(state, &auth, company) {
        return Err(resp);
    }
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return Err(resp);
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
) -> Result<Json<ChatResponse>, Response> {
    let company = CompanyId::new(&id);
    let by = chat_actor(&headers, &state, &company, peer).await?;
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    chat_and_emit(&state, &company, runtime, message, by)
        .await
        .map_err(IntoResponse::into_response)
}

/// `POST /api/v1/company/chat` (single-company alias).
async fn operator_chat_single(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(message): Json<ChatMessage>,
) -> Result<Json<ChatResponse>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    let id = runtime.id().clone();
    let by = chat_actor(&headers, &state, &id, peer).await?;
    chat_and_emit(&state, &id, runtime, message, by)
        .await
        .map_err(IntoResponse::into_response)
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
            steps: view.steps,
            task_id: view.task_id,
            parent_id: view.parent_id,
            reactions: view
                .reactions
                .into_iter()
                .map(ChatReactionDto::from)
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

/// Resolves who is reading a desk's history, for the `mine` flag. Reuses
/// [`chat_actor`]'s auth (session cookie or platform credential, tenant
/// address-authorization, temporary-password gate) so a history read can
/// never see more than a matching chat send could.
async fn history_viewer(
    headers: &HeaderMap,
    state: &AppState,
    company: &CompanyId,
    peer: Option<std::net::SocketAddr>,
) -> Result<Viewer, Response> {
    let actor = chat_actor(headers, state, company, peer).await?;
    Ok(match actor {
        Some(actor) if actor.kind == ActorKind::User => Viewer::User(actor.id),
        _ => Viewer::Operator,
    })
}

/// Shared body for both scope forms of `GET .../chat/history`.
async fn chat_history_response(
    state: &AppState,
    company: &CompanyId,
    runtime: Arc<CompanyRuntime>,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    query: ChatHistoryQuery,
) -> Result<Json<Vec<ChatHistoryMessageDto>>, Response> {
    let viewer = history_viewer(headers, state, company, peer).await?;
    let (desk_id, desk_name) = resolve_desk(&runtime, query.desk.as_deref())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let limit = query
        .limit
        .unwrap_or(CHAT_HISTORY_PAGE_LIMIT)
        .min(CHAT_HISTORY_PAGE_LIMIT);
    let messages = history_for_desk(&runtime, &desk_id, &desk_name, &viewer, query.before, limit)
        .await
        .map_err(|e| ApiError(e).into_response())?;
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
) -> Result<Json<Vec<ChatHistoryMessageDto>>, Response> {
    let company = CompanyId::new(&id);
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    chat_history_response(&state, &company, runtime, &headers, peer, query).await
}

/// `GET /api/v1/company/chat/history` (single-company alias).
async fn chat_history_single(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Query(query): Query<ChatHistoryQuery>,
) -> Result<Json<Vec<ChatHistoryMessageDto>>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
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
) -> Result<Json<AttributionAuditDto>, Response> {
    let _viewer = history_viewer(headers, state, company, peer).await?;
    let record = runtime
        .store()
        .load(runtime.id())
        .await
        .map_err(|e| ApiError(e).into_response())?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(company.to_string())).into_response()
        })?;
    let audit = channel_attributed_replies(&runtime, &record)
        .await
        .map_err(|e| ApiError(e).into_response())?;
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
) -> Result<Json<AttributionAuditDto>, Response> {
    let company = CompanyId::new(&id);
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    attribution_audit_response(&state, &company, runtime, &headers, peer).await
}

/// `GET /api/v1/company/chat/attribution-audit` (single-company alias).
async fn attribution_audit_single(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
) -> Result<Json<AttributionAuditDto>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
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
) -> Result<StatusCode, Response> {
    let by = chat_actor(headers, state, company, peer).await?;
    let message_seq = parse_message_id(&seq).map_err(IntoResponse::into_response)?;
    validate_emoji(&body.emoji).map_err(IntoResponse::into_response)?;
    // The target must be a message. Without this the route would happily hang a
    // reaction off an approval, a lifecycle change, or a sequence position that
    // has never existed — none of which any reader could render, and all of
    // which would sit in the log forever claiming otherwise.
    let target = runtime
        .events()
        .read_from(company, message_seq, 1)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let is_message = target
        .first()
        .filter(|stored| stored.seq == message_seq)
        .is_some_and(|stored| {
            matches!(
                stored.event,
                CompanyEvent::OperatorMessage { .. } | CompanyEvent::AgentReply { .. }
            )
        });
    if !is_message {
        return Err(
            ApiError(OpenCompanyError::NotFound(format!("no chat message {seq}"))).into_response(),
        );
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
        .await
        .map_err(|e| ApiError(e).into_response())?;
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
) -> Result<StatusCode, Response> {
    let company = CompanyId::new(&id);
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    react_to_message(&state, &company, runtime, &headers, peer, seq, body).await
}

/// `POST /api/v1/company/chat/messages/{seq}/reactions` (single-company alias).
async fn react_to_message_single(
    State(state): State<AppState>,
    Path(seq): Path<String>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(body): Json<ReactionBody>,
) -> Result<StatusCode, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    let id = runtime.id().clone();
    react_to_message(&state, &id, runtime, &headers, peer, seq, body).await
}

/// `GET /api/v1/companies/{id}/approvals`.
async fn list_approvals(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ApprovalSummary>>, Response> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp);
    }
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    // Membership got you the list; role decides whether you may read what is in
    // it (issue #618).
    Ok(Json(crate::server::approval_visibility::for_principal(
        &auth,
        runtime.pending_approvals(),
    )))
}

/// `GET /api/v1/company/approvals` (single-company alias).
async fn list_approvals_single(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<ApprovalSummary>>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    // The sole company IS the addressed one, so the principal is checked
    // against it exactly as on the `{id}` form.
    if let Some(resp) = authorize_address(&state, &auth, runtime.id()) {
        return Err(resp);
    }
    // Same contents rule as the `{id}` form (issue #618) — the two handlers are
    // the same read behind two addressing forms, and a redaction applied to one
    // of them would be a hole rather than a boundary.
    Ok(Json(crate::server::approval_visibility::for_principal(
        &auth,
        runtime.pending_approvals(),
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
    /// An optional operator note (reserved; not yet surfaced to the brain).
    #[allow(dead_code)]
    #[serde(default)]
    note: Option<String>,
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
/// * **with a deny** — a scope describes what an approval grants, and a deny
///   grants nothing. Honouring one would be inventing consent out of a refusal.
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
            if body.verdict == Verdict::Deny {
                return Err(bad("a scope cannot accompany a deny verdict"));
            }
            if body.amended_payload.is_some() {
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
}

impl From<crate::runtime::grants::StandingGrant> for StandingGrantDto {
    fn from(g: crate::runtime::grants::StandingGrant) -> Self {
        Self {
            id: g.id.to_string(),
            agent: g.agent,
            tool: g.tool,
            granted_by: g.granted_by,
            at_millis: g.at_millis,
            expires_at_millis: g.expires_at_millis,
            scope: g.scope,
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
        })
        .into_response());
    }

    let report = crate::company::runtime::join_follow_up(follow_up).await?;
    emit_cycle_webhooks(state, company, &report).await;
    Ok(Json(ChatResponse {
        message_id: None,
        responses: report.responses,
        still_awaiting: Some(still_awaiting),
    })
    .into_response())
}

/// `POST /api/v1/companies/{id}/approvals/{aid}`.
async fn resolve_approval(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, String)>,
    Json(body): Json<ResolveApproval>,
) -> Result<Response, Response> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp);
    }
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    let actor = resolving_actor(auth);
    run_resolve(&state, &company, runtime, aid, body, actor)
        .await
        .map_err(IntoResponse::into_response)
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
) -> Result<Response, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    let id = runtime.id().clone();
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return Err(resp);
    }
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return Err(resp);
    }
    let actor = resolving_actor(auth);
    run_resolve(&state, &id, runtime, aid, body, actor)
        .await
        .map_err(IntoResponse::into_response)
}

#[cfg(test)]
mod test {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::company::CompanyManifest;
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
        // Pre-seed a record so the builder preserves the requested lifecycle.
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        use crate::ports::CompanyStore;
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest(),
                ledger: Vec::new(),
                lifecycle: lifecycle.to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();

        let mut builder = RuntimeBuilder::new(home.to_path_buf(), manifest()).with_id(id.clone());
        if let Some(brain) = brain {
            builder = builder.with_brain(brain);
        }
        let runtime = builder.build().await.unwrap();
        let state = AppState::new(config);
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
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["responses"][0]["text"], "You said: hi");
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
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
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

    /// One chat request, optionally addressed to a thread.
    fn chat_to(text: &str, chat: Option<&str>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/company/chat")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "text": text, "chat": chat }).to_string(),
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
            tasks[0].origin_chat_id.as_deref(),
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
        assert_eq!(
            unaddressed.origin_chat_id, None,
            "an unaddressed message has no thread to answer in"
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
                text: ask.to_string(),
                chat: None,
                parent: None,
                deliverable: None,
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
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        };
        FsCompanyStore::new(home.to_path_buf())
            .save(&record)
            .await
            .unwrap();

        let deps = HarnessDeps {
            ledgers: None,
            ledger_registry: Default::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(home.to_path_buf())),
            store: Arc::new(FsCompanyStore::new(home.to_path_buf())),
            meter: Some(Arc::new(FsOps::new(home.to_path_buf()))),
            workspace_root: home.to_path_buf(),
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
            workspace: None,
            repos: None,
            repo_bindings: Vec::new(),
            checkouts: crate::harness::repo::CheckoutLedger::default(),
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
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let text = value["responses"][0]["text"].as_str().unwrap();
        // The mock provider's `mock: ` prefix proves the message went through an
        // openhuman agent turn; the trailing `hi` is the operator message the
        // agent forwarded (the agent prepends a date/time context line). Crucially
        // it is NOT the echo brain's `"You said: hi"`.
        assert!(text.starts_with("mock: "), "not an agent reply: {text:?}");
        assert!(
            text.trim_end().ends_with("hi"),
            "message not forwarded: {text:?}"
        );
        assert_ne!(text, "You said: hi", "still routing through the echo brain");
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
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
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
        let desks = get_desks(&app, &cookie).await;
        let arr = desks.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "studio"); // manifest desk first
        assert_eq!(arr[1]["id"], "growth_desk");
        assert_eq!(arr[1]["overlayCreated"], true);
    }

    /// Create-desk validation: an empty name is 400, an id colliding with a
    /// manifest desk is 409, and an unknown member is 400.
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
        assert_eq!(desks.as_array().unwrap().len(), 1);
        assert_eq!(desks[0]["id"], "studio");

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
        // The default test manifest defines no group chats, so the route answers
        // 200 with an empty list (the console then falls back to its defaults).
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
        assert_eq!(value.as_array().unwrap().len(), 0);
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

        for (text, task_id) in [
            ("opened one", Some("t-77".to_string())),
            ("just talking", None),
        ] {
            runtime
                .events()
                .append(
                    runtime.id(),
                    CompanyEvent::AgentReply {
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
            serde_json::json!({ "recorded": true, "alreadyResolved": false, "stillAwaiting": 0 })
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
            serde_json::json!({ "recorded": true, "alreadyResolved": true, "stillAwaiting": 0 })
        );
        assert_eq!(
            c.runtime.grants.live_count(),
            1,
            "re-approving minted no second grant"
        );
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
            serde_json::json!({ "recorded": true, "alreadyResolved": false, "stillAwaiting": 0 })
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
        let value = super::project_stream_item(&EventStreamItem::Gap { missed: 44 })
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

    /// A threaded reply carries its parent onto the live frame (issue #364), so
    /// a console watching the stream folds it under the same row a reload would
    /// — otherwise a thread answer arrives live in the channel and then jumps
    /// into the thread on the next refresh.
    #[test]
    fn projects_agent_reply_with_its_thread_parent() {
        let v = super::project_event(&stored(CompanyEvent::AgentReply {
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
                text: "the operator's own words".into(),
                by: None,
                chat: Some("General".into()),
                parent: None,
                deliverable: None,
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
        }))
        .expect("desk_task_completed is an attention signal");
        assert!(v.get("chatId").is_none(), "{v}");
        assert_eq!(v["column"], serde_json::json!("in_review"));
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
        }))
        .expect("workflow_run_started reaches the console");
        assert_eq!(started["type"], "workflow_run_started");
        assert_eq!(started["workflowId"], "digest");
        assert_eq!(started["runId"], "run-1");
        assert_eq!(started["scheduled"], true);

        let node = super::project_event(&stored(CompanyEvent::WorkflowNodeFinished {
            workflow_id: "digest".into(),
            run_id: "run-1".into(),
            node_id: "ceo".into(),
            status: crate::ports::types::WorkflowNodeStatus::Error,
            elapsed_ms: 1234,
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
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
                deliverable: None,
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
    #[tokio::test]
    async fn a_contradictory_or_unbounded_scope_is_refused() {
        let home_dir = home();
        let state = state_with_company(home_dir.path(), "running").await;

        let day: u64 = 24 * 60 * 60 * 1000;
        for (label, body) in [
            (
                "a scope cannot ride a deny",
                format!(r#"{{"verdict":"deny","scope":"tool","expires_in_millis":{day}}}"#),
            ),
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
                tool: "workspace_write".into(),
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
                        responses.push(crate::ports::types::OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: grant.agent.clone(),
                            agent: None,
                            text: format!("re-issued {approval_id}"),
                            steps: Vec::new(),
                            reply_to: None,
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

    fn approve_detached(id: &ApprovalId) -> Request<Body> {
        resolve_request(id, serde_json::json!({"verdict":"approve","detach":true}))
    }

    /// Waits for the follow-up work a detached resolve spawned to settle.
    async fn settle(runtime: &Arc<CompanyRuntime>) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while runtime.continuations.waiting() > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the turn never unblocked");
        // The continuation itself runs on a spawned task; let it finish.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
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
        settle(&c.runtime).await;

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
        settle(&c.runtime).await;

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
        settle(&c.runtime).await;

        let replies = agent_replies(&c.runtime).await;
        assert_eq!(replies.len(), 2, "both re-issues answered");
        assert!(
            replies.iter().all(|r| r.starts_with("sales|")),
            "the continuation must land in the channel the approval was raised in, got {replies:?}"
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
            settle(&c.runtime).await;
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
        settle(&c.runtime).await;

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
        settle(&c.runtime).await;

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
    }
}
