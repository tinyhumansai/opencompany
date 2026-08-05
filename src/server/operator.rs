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

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, EventSeq, OutboundMessage, OverlayDesk,
    OverlayDeskMember, OverlayDeskOrder, StoredEvent, TurnStep, Verdict,
};
use crate::runtime::grants::{GrantId, GrantScope, MAX_STANDING_GRANT_MILLIS};
use crate::runtime::types::{ApprovalSummary, CompanyStatus, CycleReport};
use crate::server::chat_history::{MessageView, ReactionView, Viewer, history_for_desk};
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
/// Validates that the desk exists in the manifest and that `agent_id` resolves
/// to a roster teammate (a manifest agent or a team-overlay teammate); rejects
/// with `404`/`400` otherwise. Adding a teammate already on the desk (manifest
/// or overlay) is a `409`.
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
    // The desk must be one of the company's blueprint group chats.
    if !record.manifest.group_chats.iter().any(|c| c.id == desk_id) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
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
    // First validate that the desk exists in the manifest — otherwise a caller
    // supplying an unknown desk_id gets a desk-scoped 404 rather than a confusing
    // member-scoped one (Greptile feedback).
    if !record.manifest.group_chats.iter().any(|c| c.id == desk_id) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
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
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
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
        .filter_map(move |stored| {
            // Keep the teardown guard alive for the life of the stream.
            let _ = &guard;
            let event =
                project_event(&stored).map(|value| Ok(Event::default().data(value.to_string())));
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
        // The dispatch terminal (#185). `output` is the agent's own reply text
        // — the same string already written into the card's note — never raw
        // tool output, so it is safe to project.
        CompanyEvent::DeskTaskCompleted {
            task_id,
            desk,
            output,
            column,
            ..
        } => {
            let mut o = envelope("desk_task_completed");
            o["taskId"] = json!(task_id);
            o["desk"] = json!(desk);
            o["output"] = json!(output);
            o["column"] = json!(column);
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
        CompanyEvent::ApprovalResolved {
            approval_id,
            verdict,
            ..
        } => {
            let mut o = envelope("approval_resolved");
            o["approvalId"] = json!(approval_id.as_ref());
            o["verdict"] = json!(verdict);
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
}

/// Runs one operator-chat cycle, returning the report and, when a complaint
/// intent captured feedback, the note that was captured (so the caller can emit
/// the `feedback.created` webhook).
async fn run_chat(
    runtime: Arc<CompanyRuntime>,
    message: ChatMessage,
    by: Option<Actor>,
    parent: Option<EventSeq>,
) -> Result<(CycleReport, Option<String>), ApiError> {
    runtime.ensure_running().await?;
    // Operator-chat feedback intent: a complaint phrase ("that was wrong — flag
    // it") captures a feedback item alongside the normal cycle. Neutral chat
    // carries no intent, so ordinary messages are untouched.
    let feedback_note = if let Some(category) = crate::feedback::detect_chat_intent(&message.text) {
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
    // Deterministic task card: an actionable operator request ("build the
    // landing page", "can you set up the newsletter") opens a `todo` card so
    // "do X" always leaves a visible work item on the dashboard — independent of
    // whether the orchestrator model also calls `spawn_task` (it may open
    // sub-tasks on top). Pure questions, greetings, and acknowledgements don't
    // fire, so the board fills with work, not small talk. Best-effort: a card
    // write failure must never sink the chat reply.
    if let Some(title) = crate::company::task_intent::detect_task_intent(&message.text) {
        // Keep the full message as the note only when the title was shortened
        // from it, so a one-line ask doesn't duplicate itself.
        let note =
            (title.trim_end_matches('…') != message.text.trim()).then(|| message.text.clone());
        let record = crate::ports::tasks::TaskRecord {
            id: crate::ports::generate_id(),
            title,
            note,
            column: crate::ports::tasks::COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: String::new(),
            updated_at_millis: crate::ports::now_millis(),
            origin_chat_id: None,
            parent_task_id: None,
        };
        if let Err(err) = runtime.upsert_task(&record).await {
            tracing::warn!(error = %err, "failed to open task card for chat request");
        }
    }
    let report = runtime
        .run_cycle(vec![CompanyEvent::OperatorMessage {
            text: message.text,
            by,
            // Thread the addressed desk through so the orchestrator brain can
            // route to that desk's lead member (issue #53).
            chat: message.chat,
            // …and the message being replied to, so the thread is a fact about
            // the transcript rather than about one browser (issue #364).
            parent,
        }])
        .await?;
    Ok((report, feedback_note))
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
    let (mut report, feedback_note) = run_chat(runtime.clone(), message, by, parent).await?;
    emit_cycle_webhooks(state, id, &report).await;
    if let Some(note) = feedback_note {
        emit_feedback_webhook(state, id, &note).await;
    }
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
                    chat_id: desk.clone(),
                    agent_id: response.channel.clone(),
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
    Ok(Json(ChatResponse {
        // The operator's own message is the cycle's single input event, so its
        // sequence is the first the cycle journaled (issue #364).
        message_id: report.input_seqs.first().map(|seq| seq.value().to_string()),
        responses: report.responses,
    }))
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
) -> Result<Option<Actor>, Response> {
    use crate::server::graphql::auth::{GqlAuth, resolve_principal};

    let auth = resolve_principal(headers, state, Some(company))
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
    Json(message): Json<ChatMessage>,
) -> Result<Json<ChatResponse>, Response> {
    let company = CompanyId::new(&id);
    let by = chat_actor(&headers, &state, &company).await?;
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    chat_and_emit(&state, &company, runtime, message, by)
        .await
        .map_err(IntoResponse::into_response)
}

/// `POST /api/v1/company/chat` (single-company alias).
async fn operator_chat_single(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(message): Json<ChatMessage>,
) -> Result<Json<ChatResponse>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    let id = runtime.id().clone();
    let by = chat_actor(&headers, &state, &id).await?;
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

/// How many messages `GET .../chat/history` returns. Generous enough to
/// hydrate a console thread on load (issue #65) while still bounding the
/// response on a very long transcript; pagination is a GraphQL `Chat.history`
/// concern, not this REST convenience route's.
const CHAT_HISTORY_LIMIT: usize = 200;

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
) -> Result<Viewer, Response> {
    let actor = chat_actor(headers, state, company).await?;
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
    query: ChatHistoryQuery,
) -> Result<Json<Vec<ChatHistoryMessageDto>>, Response> {
    let viewer = history_viewer(headers, state, company).await?;
    let (desk_id, desk_name) = resolve_desk(&runtime, query.desk.as_deref())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let (messages, _total) = history_for_desk(
        &runtime,
        &desk_id,
        &desk_name,
        &viewer,
        None,
        CHAT_HISTORY_LIMIT,
    )
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
    Query(query): Query<ChatHistoryQuery>,
) -> Result<Json<Vec<ChatHistoryMessageDto>>, Response> {
    let company = CompanyId::new(&id);
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    chat_history_response(&state, &company, runtime, &headers, query).await
}

/// `GET /api/v1/company/chat/history` (single-company alias).
async fn chat_history_single(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ChatHistoryQuery>,
) -> Result<Json<Vec<ChatHistoryMessageDto>>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    let id = runtime.id().clone();
    chat_history_response(&state, &id, runtime, &headers, query).await
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
    seq: String,
    body: ReactionBody,
) -> Result<StatusCode, Response> {
    let by = chat_actor(headers, state, company).await?;
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
    Json(body): Json<ReactionBody>,
) -> Result<StatusCode, Response> {
    let company = CompanyId::new(&id);
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    react_to_message(&state, &company, runtime, &headers, seq, body).await
}

/// `POST /api/v1/company/chat/messages/{seq}/reactions` (single-company alias).
async fn react_to_message_single(
    State(state): State<AppState>,
    Path(seq): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ReactionBody>,
) -> Result<StatusCode, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    let id = runtime.id().clone();
    react_to_message(&state, &id, runtime, &headers, seq, body).await
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
    Ok(Json(runtime.pending_approvals()))
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
    Ok(Json(runtime.pending_approvals()))
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
        })
        .into_response());
    }

    let report = crate::company::runtime::join_follow_up(follow_up).await?;
    emit_cycle_webhooks(state, company, &report).await;
    Ok(Json(ChatResponse {
        message_id: None,
        responses: report.responses,
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

    /// An actionable operator chat opens exactly one `todo` task card on the
    /// dashboard (deterministic, independent of the brain's own `spawn_task`),
    /// and a greeting opens none. Runs on the default echo brain, so it proves
    /// the handler-level wiring, not model behaviour.
    #[tokio::test]
    async fn actionable_chat_opens_a_todo_task_card() {
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

        // Actionable → one To-do card, titled from the ask.
        let r = app
            .clone()
            .oneshot(chat("build the landing page"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1, "an actionable ask opens one card");
        assert_eq!(tasks[0].column, crate::ports::tasks::COLUMN_TODO);
        assert_eq!(tasks[0].priority, "medium");
        assert_eq!(tasks[0].title, "Build the landing page");

        // Greeting → no new card.
        let r = app.oneshot(chat("thanks!")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1, "a greeting must not open a card");
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
            template_provenance: None,
        };
        FsCompanyStore::new(home.to_path_buf())
            .save(&record)
            .await
            .unwrap();

        let deps = HarnessDeps {
            run_supervisor: crate::runtime::RunSupervisor::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(home.to_path_buf())),
            store: Arc::new(FsCompanyStore::new(home.to_path_buf())),
            meter: Some(Arc::new(FsOps::new(home.to_path_buf()))),
            workspace_root: home.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: crate::harness::orchestrator::DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
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
    fn gated_tool_call() -> crate::ports::types::Effect {
        crate::ports::types::Effect {
            kind: "composio_execute".into(),
            group: crate::ports::types::EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" }),
            agent: Some("ceo".into()),
            run_id: None,
        }
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
                        host.park_effect(gated_tool_call()).await?;
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
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let state = build_state_with_brain(
            home,
            "running",
            AppConfig::default(),
            Some(Arc::new(StalledContinuationBrain {
                entered: entered.clone(),
                release: release.clone(),
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
            serde_json::json!({ "recorded": true, "alreadyResolved": false })
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
            serde_json::json!({ "recorded": true, "alreadyResolved": true })
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
            serde_json::json!({ "recorded": true, "alreadyResolved": false })
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

    /// #185: the dispatch terminal projects all four fields. `column` is the one
    /// that matters most — it is how a console tells a clean finish from a
    /// cancelled or failed run without parsing `output`.
    #[test]
    fn projects_desk_task_completed_with_every_field() {
        let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".into(),
            desk: "ceo".into(),
            output: "shipped".into(),
            column: "in_review".into(),
            artifact_ids: Vec::new(),
        }))
        .expect("desk_task_completed is an attention signal");
        assert_eq!(v["type"], serde_json::json!("desk_task_completed"));
        assert_eq!(v["taskId"], serde_json::json!("t-1"));
        assert_eq!(v["desk"], serde_json::json!("ceo"));
        assert_eq!(v["output"], serde_json::json!("shipped"));
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
        }))
        .expect("workflow_run_finished is an attention signal");
        assert_eq!(v["error"], "no inference source for agent node `worker`");
        assert_eq!(v["deliveries"].as_array().unwrap().len(), 0);
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
}
