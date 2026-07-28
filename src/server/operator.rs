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
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::StreamExt;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, OutboundMessage, OverlayDesk,
    OverlayDeskMember, StoredEvent, TurnStep, Verdict,
};
use crate::runtime::types::{ApprovalSummary, CompanyStatus, CycleReport};
use crate::server::chat_history::{MessageView, Viewer, history_for_desk};
use crate::server::error::ApiError;
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
        .route("/api/v1/companies/{id}/approvals", get(list_approvals))
        .route(
            "/api/v1/companies/{id}/approvals/{aid}",
            post(resolve_approval),
        )
        // Single-company aliases (no id; resolved via the sole registered company).
        .route("/api/v1/company/chat", post(operator_chat_single))
        .route("/api/v1/company/chat/history", get(chat_history_single))
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
        // The company → operator attention feed (issue #66): a live SSE stream of
        // the attention-worthy events already on the company's event log, under
        // both scope forms.
        .merge(scoped("/events", get(company_events)))
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
    /// with operator-added overlay members (issue #72). The first is its lead.
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
/// `ScheduleFired`, `FeedbackFiled`, `MemoryFactDeleted` — is dropped so nothing
/// unexpected (or secret-bearing) ever reaches the console. The actor (`by`) on
/// `ApprovalResolved` / `LifecycleChanged` is intentionally omitted: the console
/// renders the attention item without it, and it can carry a user id.
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
        } => {
            let mut o = envelope("agent_reply");
            o["chatId"] = json!(chat_id);
            o["agentId"] = json!(agent_id);
            o["text"] = json!(text);
            // Scrubbed timeline (same shape the POST body carries); omitted
            // when empty so a tool-less reply's wire form is unchanged.
            if !steps.is_empty() {
                o["steps"] = json!(steps);
            }
            o
        }
        CompanyEvent::TaskDispatched { task_id } => {
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
        } => {
            let mut o = envelope("mcp_call_failed");
            o["server"] = json!(server);
            o["tool"] = json!(tool);
            o["status"] = json!(status);
            o["message"] = json!(message);
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
}

/// A chat or approval-resolution response: the company's channel replies.
#[derive(Debug, Serialize)]
struct ChatResponse {
    /// Channel responses produced by the cycle.
    responses: Vec<OutboundMessage>,
}

/// Runs one operator-chat cycle, returning the report and, when a complaint
/// intent captured feedback, the note that was captured (so the caller can emit
/// the `feedback.created` webhook).
async fn run_chat(
    runtime: Arc<CompanyRuntime>,
    message: ChatMessage,
    by: Option<Actor>,
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
    // landing page", "can you set up the newsletter") opens a `backlog` card so
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
            column: "backlog".to_string(),
            priority: "medium".to_string(),
            assignee: String::new(),
            updated_at_millis: crate::ports::now_millis(),
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
    let (report, feedback_note) = run_chat(runtime.clone(), message, by).await?;
    emit_cycle_webhooks(state, id, &report).await;
    if let Some(note) = feedback_note {
        emit_feedback_webhook(state, id, &note).await;
    }
    // Journal each reply against the addressed desk so desk history can be read
    // back (GraphQL `Chat.history`, WS2c). Single-responder in v1.
    for response in &report.responses {
        let _ = runtime
            .events()
            .append(
                id,
                CompanyEvent::AgentReply {
                    chat_id: desk.clone(),
                    agent_id: response.channel.clone(),
                    text: response.text.clone(),
                    // Persist the per-bubble timeline so a history reload
                    // rehydrates the tool calls, not just the text.
                    steps: response.steps.clone(),
                },
            )
            .await;
    }
    Ok(Json(ChatResponse {
        responses: report.responses,
    }))
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
}

async fn run_resolve(
    state: &AppState,
    company: &CompanyId,
    runtime: Arc<CompanyRuntime>,
    approval_id: String,
    body: ResolveApproval,
) -> Result<Json<ChatResponse>, ApiError> {
    runtime.ensure_running().await?;
    let actor = Actor {
        kind: ActorKind::Operator,
        id: "operator".to_string(),
    };
    let id = ApprovalId::new(approval_id);
    let report = match (body.verdict, body.amended_payload) {
        (Verdict::Approve, Some(payload)) => {
            runtime
                .resolve_approval_amended(&id, payload, actor)
                .await?
        }
        (Verdict::Deny, Some(_)) => {
            return Err(ApiError(OpenCompanyError::InvalidRequest(
                "amended_payload cannot accompany a deny verdict".to_string(),
            )));
        }
        (verdict, None) => runtime.resolve_approval(&id, verdict, actor).await?,
    };
    emit_cycle_webhooks(state, company, &report).await;
    Ok(Json(ChatResponse {
        responses: report.responses,
    }))
}

/// `POST /api/v1/companies/{id}/approvals/{aid}`.
async fn resolve_approval(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, String)>,
    Json(body): Json<ResolveApproval>,
) -> Result<Json<ChatResponse>, Response> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp);
    }
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    run_resolve(&state, &company, runtime, aid, body)
        .await
        .map_err(IntoResponse::into_response)
}

/// `POST /api/v1/company/approvals/{aid}` (single-company alias).
async fn resolve_approval_single(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(aid): Path<String>,
    Json(body): Json<ResolveApproval>,
) -> Result<Json<ChatResponse>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    let id = runtime.id().clone();
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return Err(resp);
    }
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return Err(resp);
    }
    run_resolve(&state, &id, runtime, aid, body)
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

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("opencompany-http-{}", crate::ports::generate_id()))
    }

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
    }

    async fn state_with_company(home: &std::path::Path, lifecycle: &str) -> AppState {
        build_state(home, lifecycle, AppConfig::default()).await
    }

    async fn build_state(home: &std::path::Path, lifecycle: &str, config: AppConfig) -> AppState {
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
                overlay_desks: Vec::new(),
            })
            .await
            .unwrap();

        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(config);
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    #[tokio::test]
    async fn chat_returns_echoed_response() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// An actionable operator chat opens exactly one `backlog` task card on the
    /// dashboard (deterministic, independent of the brain's own `spawn_task`),
    /// and a greeting opens none. Runs on the default echo brain, so it proves
    /// the handler-level wiring, not model behaviour.
    #[tokio::test]
    async fn actionable_chat_opens_a_backlog_task_card() {
        let home = home();
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

        // Actionable → one backlog card, titled from the ask.
        let r = app
            .clone()
            .oneshot(chat("build the landing page"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1, "an actionable ask opens one card");
        assert_eq!(tasks[0].column, "backlog");
        assert_eq!(tasks[0].priority, "medium");
        assert_eq!(tasks[0].title, "Build the landing page");

        // Greeting → no new card.
        let r = app.oneshot(chat("thanks!")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let tasks = runtime.tasks().list(&id).await.unwrap();
        assert_eq!(tasks.len(), 1, "a greeting must not open a card");

        tokio::fs::remove_dir_all(&home).await.ok();
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

        let home = home();
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
            overlay_desks: Vec::new(),
        };
        FsCompanyStore::new(home.to_path_buf())
            .save(&record)
            .await
            .unwrap();

        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(home.to_path_buf())),
            store: Arc::new(FsCompanyStore::new(home.to_path_buf())),
            meter: Some(Arc::new(FsOps::new(home.to_path_buf()))),
            workspace_root: home.to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: crate::harness::orchestrator::DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
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
        tokio::fs::remove_dir_all(&home).await.ok();
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
                overlay_desks: Vec::new(),
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
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// Removing an overlay member drops it from the merged view; a manifest
    /// member cannot be removed (409), and an unknown overlay member is a 404.
    #[tokio::test]
    async fn remove_desk_member_drops_overlay_and_guards_manifest() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// Creating a desk persists it as an overlay and surfaces it in `list_desks`
    /// alongside the manifest desks, flagged `overlayCreated` with its lead
    /// first. The manifest is never rewritten.
    #[tokio::test]
    async fn create_desk_persists_and_appears_in_list() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// Create-desk validation: an empty name is 400, an id colliding with a
    /// manifest desk is 409, and an unknown member is 400.
    #[tokio::test]
    async fn create_desk_validates_name_id_and_members() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// Deleting an operator-created desk drops it (and any of its overlay
    /// members); a manifest desk cannot be deleted (409); an unknown id is 404.
    #[tokio::test]
    async fn delete_desk_removes_overlay_and_guards_manifest() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// Add-member validation: an unknown desk is 404, an unknown teammate is
    /// 400, and a teammate already on the desk is 409.
    #[tokio::test]
    async fn add_desk_member_validates_desk_agent_and_duplicates() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn desks_route_returns_the_company_desks() {
        // The default test manifest defines no group chats, so the route answers
        // 200 with an empty list (the console then falls back to its defaults).
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// Issue #65: the console's default thread addresses sends with
    /// `chat: "main"`, but pre-threading history and the synthetic operator
    /// desk are keyed on `"General"`. A transcript spanning both ids — one
    /// operator turn journaled under each — must read back as one history via
    /// the REST route with no `?desk=` selector (the console's default read).
    #[tokio::test]
    async fn chat_history_route_reunifies_general_and_main_transcripts() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

        runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::AgentReply {
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// Regression: a reply's tool-call timeline must survive a history reload —
    /// switching threads and coming back reloads `chat/history`, which used to
    /// return text only, so the steps vanished. They are now persisted on the
    /// `AgentReply` and projected back through the DTO.
    #[tokio::test]
    async fn chat_history_route_rehydrates_reply_steps() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

        runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::AgentReply {
                    chat_id: "main".to_string(),
                    agent_id: "ceo".to_string(),
                    text: "done".to_string(),
                    steps: vec![TurnStep {
                        kind: crate::ports::types::TurnStepKind::ToolCall,
                        status: crate::ports::types::TurnStepStatus::Ok,
                        label: "Reading messages".to_string(),
                        detail: None,
                        elapsed_ms: Some(9),
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// A desk id with no `?desk=` selector defaults to the operator/General
    /// thread; an unaddressed thread id that neither matches a manifest desk
    /// nor the General desk reads back empty rather than erroring.
    #[tokio::test]
    async fn chat_history_route_unknown_desk_is_empty_not_an_error() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn chat_by_id_matches_registered_company() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn unknown_company_is_404() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn paused_company_chat_is_409() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn list_and_status_routes_report_the_company() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn approvals_list_is_empty_before_any_park() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn amended_approve_resolves_and_returns_responses() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn deny_with_amended_payload_is_400() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/approvals/missing")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"verdict":"deny","amended_payload":{"text":"edited"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["code"], "invalid_request");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn a_session_is_required_and_sufficient() {
        // Replaces `operator_token_guards_routes`. That token could never be
        // set, so the test only ever proved the guard worked in a state no
        // deployment could reach; every real host served this route to anyone.
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
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
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "shipped it".into(),
            steps: vec![TurnStep {
                kind: TurnStepKind::ToolCall,
                status: TurnStepStatus::Ok,
                label: "Reading messages".into(),
                detail: None,
                elapsed_ms: Some(12),
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
    }

    #[test]
    fn projects_agent_reply_omits_empty_steps() {
        let v = super::project_event(&stored(CompanyEvent::AgentReply {
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "hi".into(),
            steps: Vec::new(),
        }))
        .expect("agent_reply is an attention signal");
        // A tool-less reply keeps the legacy wire shape — no `steps` key.
        assert!(v.get("steps").is_none());
    }

    #[test]
    fn projects_task_dispatched() {
        let v = super::project_event(&stored(CompanyEvent::TaskDispatched {
            task_id: "t-42".into(),
        }))
        .expect("task_dispatched is an attention signal");
        assert_eq!(v["type"], "task_dispatched");
        assert_eq!(v["taskId"], "t-42");
    }

    #[test]
    fn projects_mcp_call_failed_with_scrubbed_message() {
        let v = super::project_event(&stored(CompanyEvent::McpCallFailed {
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

    #[test]
    fn drops_non_attention_and_raw_payload_events() {
        // The operator's own message, and every variant that carries a raw
        // third-party payload or is audit-only, is dropped so nothing unexpected
        // (or secret-bearing) ever reaches the console.
        let dropped = [
            CompanyEvent::OperatorMessage {
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
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn events_route_requires_a_session() {
        let home = home();
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
        tokio::fs::remove_dir_all(&home).await.ok();
    }
}
