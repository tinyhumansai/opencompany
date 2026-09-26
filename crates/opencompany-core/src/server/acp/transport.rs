//! HTTP transport for the host-side ACP session model.
//!
//! ACP's native transports are stdio and WebSocket JSON-RPC. OpenCompany uses
//! HTTP JSON-RPC at its public edge: one request carries one RPC call, while
//! the returned `updates` array preserves the protocol's ordered session
//! updates for callers that cannot hold a socket open. The endpoint is always
//! authenticated with the same company authorization as the operator API.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::AppState;
use crate::ports::types::{Actor, ActorKind, CompanyEvent, CompanyId};
use crate::server::graphql::auth::GqlAuth;
use crate::server::platform_auth::{CompanyAuth, authorize_address, refuse_until_password_changed};

/// The ACP protocol version this host speaks.
const PROTOCOL_VERSION: u64 = 1;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/acp", post(call))
}

async fn call(
    State(state): State<AppState>,
    CompanyAuth(auth): CompanyAuth,
    Json(request): Json<Value>,
) -> Response {
    // A temporary password is a boundary, not a suggestion: a user who has not
    // replaced it may not run company cycles over any surface, ACP included.
    // The same check `ScopedCompany` applies to the operator API.
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return resp;
    }
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(initialize_result()),
        "session/new" => open_session(&state, &auth, &params).await,
        "session/list" => list_sessions(&state, &auth, &params),
        "session/prompt" => prompt(&state, &auth, &params).await,
        "session/delete" => delete_session(&state, &auth, &params),
        // The HTTP edge has no socket whose closure sweeps a connection, so
        // the client ends its own connection explicitly.
        "session/disconnect" => disconnect(&state, &auth, &params),
        // There is no safe generic interruption point inside an arbitrary
        // company cycle. Say so rather than claiming a cancel that cannot stop
        // provider work or tools already in flight.
        "session/cancel" => Err(
            "OpenCompany does not yet support cancelling an in-flight company cycle".to_string(),
        ),
        _ => Err(format!("unsupported ACP method `{method}`")),
    };
    match result {
        Ok(result) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response(),
        Err(message) => Json(
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": message } }),
        )
        .into_response(),
    }
}

/// What this host answers an `initialize` with.
///
/// ACP, not MCP: `protocolVersion` is the integer ACP version and the result
/// carries `agentCapabilities` and `agentInfo`, where MCP's shape has a
/// date-valued `protocolVersion`, `capabilities` and `serverInfo`. A standard
/// ACP client (Zed, `acpx`) deserializes the ACP shape, so an MCP-shaped
/// answer would fail the handshake before any session could open.
///
/// Capabilities are what this host actually implements — `session/delete`
/// only. Everything omitted defaults to unsupported, which is the honest
/// answer rather than a promise a later turn would have to keep.
fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": { "session": { "delete": {} } },
        "agentInfo": {
            "name": "opencompany",
            "title": "OpenCompany",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn connection(params: &Value) -> Result<&str, String> {
    params
        .get("_meta")
        .and_then(|m| m.get("opencompany/connectionId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "`_meta.opencompany/connectionId` is required".to_string())
}

/// A stable identity for whoever presented `auth`, used to bind a
/// caller-supplied `connectionId` to the caller that first used it. Two
/// different users (or platform tenants) must never resolve to the same
/// owner string.
fn owner(auth: &GqlAuth) -> String {
    match auth {
        // `user_id` is only guaranteed stable within `user.company` — two
        // companies can legitimately mint the same id — so both are part of
        // the key, or a collision would let a second company's user share a
        // connection binding with the first's.
        GqlAuth::User(user) => {
            // Length-prefixed, because a plain join is not injective: neither
            // `CompanyId::new` nor a stored `UserRecord::id` forbids a `:`, so
            // (company `a:b`, user `c`) and (company `a`, user `b:c`) would
            // produce the same owner and let one address the other's
            // connection — defeating the check this key exists to make.
            let company = user.company.as_ref();
            let id = user.user_id.as_str();
            format!("user:{}:{}:{}:{}", company.len(), company, id.len(), id)
        }
        // Canonicalized for the same reason `authorize_address` compares
        // tenants in this form: `tenant:acme` and `acme` name the same
        // tenant, and a raw-string key would treat a token whose issuer
        // format changed (or was rotated to the other form) as a stranger to
        // its own still-open connection.
        GqlAuth::Platform(claims) => {
            format!("platform:{}", crate::app::canonical_tenant(&claims.tenant))
        }
    }
}

fn target(params: &Value) -> Result<(&str, Option<String>, Option<String>), String> {
    let meta = params
        .get("_meta")
        .and_then(|m| m.get("opencompany"))
        .ok_or_else(|| "`_meta.opencompany` is required".to_string())?;
    let company = meta
        .get("company")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "`_meta.opencompany.company` is required".to_string())?;
    let chat = meta
        .get("chat")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let agent = meta
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((company, chat, agent))
}

async fn open_session(state: &AppState, auth: &GqlAuth, params: &Value) -> Result<Value, String> {
    // Refused, not ignored: silently dropping `mcpServers` or
    // `additionalDirectories` would tell a client its tools and extra roots
    // were active when they never were. `session::refuse_unsupported` is the
    // single place that decision lives.
    if let Some(refusal) = super::session::refuse_unsupported(
        params
            .get("mcpServers")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        params
            .get("additionalDirectories")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    ) {
        return Err(refusal.message().to_string());
    }
    let (company, requested_chat, agent_id) = target(params)?;
    let company = CompanyId::new(company);
    if authorize_address(state, auth, &company).is_some() {
        return Err("not authorized for this company".to_string());
    }
    let runtime = state
        .registry()
        .get(&company)
        .ok_or_else(|| format!("company `{company}` was not found"))?;
    // A pin that names nobody must be refused now, not answered by the
    // orchestrator for the life of the session. `resolve_roster_agent_id` is
    // the same resolver the cycle's routing uses, so what passes here is
    // exactly what routes later.
    if let Some(id) = &agent_id {
        let record = runtime
            .store()
            .load(&company)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("company `{company}` was not found"))?;
        if record.resolve_roster_agent_id(id).is_none() {
            return Err(format!("`agentId` `{id}` is not a roster member"));
        }
    }
    let requested_chat = match requested_chat {
        Some(chat) => chat,
        None => runtime
            .default_agent_dm()
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string()),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let session = state
        .acp_sessions()
        .open(
            connection(params)?,
            &owner(auth),
            super::AcpSession {
                id,
                company,
                // A pinned session answers as its member; see `AcpSession::thread_key`.
                chat: super::AcpSession::thread_key(&requested_chat, agent_id.as_deref()),
                agent_id,
            },
            crate::ports::now_millis(),
        )
        .map_err(|refusal| refusal.message().to_string())?;
    // ACP's result requires a `cwd`. On this host the workspace is server-side
    // and the client's own path is never used — the same truth `cwd_meta`
    // reports in `_meta` is stated as `cwd` so a strict client deserializes.
    let workspace = "server-side company workspace";
    Ok(json!({
        "sessionId": session.id,
        "cwd": workspace,
        "_meta": super::session::cwd_meta(workspace),
    }))
}

fn list_sessions(state: &AppState, auth: &GqlAuth, params: &Value) -> Result<Value, String> {
    // `connectionId` is caller-supplied and shared state, not a credential: an
    // authenticated tenant who learns another tenant's connection id must not
    // be able to enumerate its company, thread, agent and session ids. Each
    // entry is therefore filtered through the same `authorize_address` every
    // other company-scoped read gets — and the connection itself must be one
    // this caller opened, checked by `SessionRegistry::list`.
    let sessions = state
        .acp_sessions()
        .list(
            connection(params)?,
            &owner(auth),
            crate::ports::now_millis(),
        )
        .ok_or_else(|| "unknown ACP connection".to_string())?
        .into_iter()
        .filter(|s| authorize_address(state, auth, &s.company).is_none())
        .map(|s| {
            json!({
                "sessionId": s.id,
                "_meta": {
                    "opencompany": {
                        "company": s.company,
                        "chat": s.chat,
                        "agentId": s.agent_id,
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "sessions": sessions }))
}

fn delete_session(state: &AppState, auth: &GqlAuth, params: &Value) -> Result<Value, String> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| "`sessionId` is required".to_string())?;
    let conn = connection(params)?;
    let owner = owner(auth);
    let registry = state.acp_sessions();
    // Authorize against the session when it exists. A never-existing session,
    // and a connection this caller does not own, both delete silently — ACP
    // says so for the former, and an id is opaque enough that saying "I never
    // had that" leaks nothing useful either way. `peek`, not `get`: deleting
    // renews nothing, so there is no reason to touch the idle TTL of a
    // session this call may yet refuse to act on.
    if let Some(session) = registry.peek(conn, &owner, session_id, crate::ports::now_millis())
        && authorize_address(state, auth, &session.company).is_some()
    {
        return Err("not authorized for this company".to_string());
    }
    registry.remove(conn, &owner, session_id);
    Ok(json!({}))
}

/// Closes the caller's connection: every session it opened whose company the
/// caller may still address, in one stroke.
///
/// The HTTP edge has no socket whose closure sweeps a connection, so the
/// client ends its connection explicitly. `SessionRegistry::close_connection`
/// checks ownership, runs `authorized` per session, and removes what passes
/// under the same lock — so there is no snapshot for a concurrent
/// `session/new` to land in after the check and survive the disconnect,
/// unlike the list-then-remove loop this replaced. The per-session check
/// stays: `owner` names a tenant, not an authorization
/// scope, and two platform credentials for the same tenant can carry
/// different company allow-lists — closing every session unconditionally
/// would let a narrowly-scoped credential remove sessions for companies
/// outside its own allow-list merely by sharing an owner string with
/// whichever credential opened them. A session whose authorization has since
/// lapsed is left for the periodic sweep instead — an hourly cadence
/// ([`SESSION_SWEEP_INTERVAL_MILLIS`](super::session::SESSION_SWEEP_INTERVAL_MILLIS)),
/// not the day-long gap a blanket removal would have closed.
fn disconnect(state: &AppState, auth: &GqlAuth, params: &Value) -> Result<Value, String> {
    let conn = connection(params)?;
    let owner = owner(auth);
    // Per-session, not blanket: `owner` names a tenant, and two platform
    // credentials for the same tenant can carry different company
    // allow-lists, so a connection's sessions may span companies the
    // presented credential is not itself authorized for.
    state
        .acp_sessions()
        .close_connection(conn, &owner, |company| {
            authorize_address(state, auth, company).is_none()
        });
    Ok(json!({}))
}

async fn prompt(state: &AppState, auth: &GqlAuth, params: &Value) -> Result<Value, String> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| "`sessionId` is required".to_string())?;
    let conn = connection(params)?;
    let owner = owner(auth);
    let registry = state.acp_sessions();
    // `peek`, not `get`: authorization can still refuse this call below, and
    // renewing the idle TTL ahead of that would let a caller whose access to
    // this session's company was revoked keep the session's cap slot alive
    // indefinitely by repeatedly presenting it and losing the check.
    let now_millis = crate::ports::now_millis();
    let session = registry
        .peek(conn, &owner, session_id, now_millis)
        .ok_or_else(|| "unknown ACP session".to_string())?;
    if authorize_address(state, auth, &session.company).is_some() {
        return Err("not authorized for this company".to_string());
    }
    registry.touch(conn, &owner, session_id, now_millis);
    let text = prompt_text(params)?;
    let runtime = state
        .registry()
        .get(&session.company)
        .ok_or_else(|| format!("company `{}` was not found", session.company))?;
    // A paused or archived company refuses work on every other surface before
    // any cycle runs (chat, A2A, webhooks); the ACP prompt must hold the same
    // line. `run_cycle` checks only the process-local quiescing window, so
    // without this an operator's explicit pause/archive would still pay for
    // provider and tool work driven here.
    runtime.ensure_running().await.map_err(|e| e.to_string())?;
    // A runtime being replaced refuses *before* the prompt is journaled. The
    // append below persists the message and its mention rows, and
    // `run_journaled_cycle` then re-checks this gate — a refusal ordered after
    // the append would leave an answered-nothing message and a durable badge
    // in the transcript. Same ordering the REST chat path holds in
    // `accept_chat_turn` (codex P2).
    runtime.ensure_accepting().map_err(|e| e.to_string())?;
    // Keep the person, drop the credential, exactly as `ScopedCompany` does: a
    // human-authored ACP prompt is attributed to that user in the journal and
    // the audit trail. Only platform credentials stay anonymous.
    let by = match auth {
        GqlAuth::User(user) => Some(Actor {
            kind: ActorKind::User,
            id: user.user_id.clone(),
        }),
        GqlAuth::Platform(_) => None,
    };
    // A pinned session is answered by its member because the thread key stored
    // at session-open was already that member's DM channel (`dm:<member>`) —
    // the one chat key the cycle's routing (`responder_for`) resolves to a
    // specific roster member. The text is sent as-is, exactly as a console DM
    // is; no synthetic `@`-mention is needed, and one would only be dropped by
    // revalidation against a body that does not contain it.
    // Both halves of one resolution, exactly as the REST chat path's
    // `accept_chat_turn` runs them: who this prompt reached, and every `@name`
    // that reached more than one thing and therefore reached nobody (B-101).
    // The ACP surface used to call the plain `resolve_mentions` here and never
    // report the ambiguous half, so a Zed (or other ACP client) operator whose
    // `@name` matched two things got the same silent non-ping the console used
    // to give before B-101, with no durable refusal notice anywhere (codex P2).
    let resolved = runtime
        .resolve_mentions_reporting(&text, None, by.as_ref())
        .await;
    let mentions = resolved.mentions.clone();
    // Journal the prompt up front so the transcript is right from acceptance
    // and the durable mention rows share its sequence — the same shape as the
    // operator `/chat` route (issue #983). `run_journaled_cycle` then runs the
    // turn on the already-recorded message instead of appending it again.
    let chat = session.chat.clone();
    // Issue #1781 review (Codex P1): this route journals straight to
    // `runtime.events()` below rather than going through the REST `/chat`
    // route's `chat_and_emit`, so it never ran that function's read-only
    // Operator-channel guard — an authenticated caller could open a session
    // with `_meta.opencompany.chat = "operator"` (or the collision-fallback
    // id) and post into the durable system feed. `ensure_desk_writable` is
    // that same guard, now shared by both write ingresses; running it here,
    // immediately before the append, is the ACP mirror of `chat_and_emit`
    // checking it before its own append.
    runtime
        .ensure_desk_writable(&chat)
        .await
        .map_err(|e| e.to_string())?;
    let event = CompanyEvent::OperatorMessage {
        text,
        by: by.clone(),
        chat: Some(chat.clone()),
        parent: None,
        deliverable: None,
        mentions,
        // ACP prompts are text-only — the wire carries no file upload — so an
        // ACP-sent message never has attachments (issue #1682).
        attachments: vec![],
    };
    // Asked again, immediately before the durable write, for the reason
    // `chat_and_emit` asks again: the check above sits behind mention and desk
    // resolution, and a stop landing in that window would leave a transcript
    // entry no turn will ever answer — and on this ingress no turn row or
    // failure event explains it either.
    runtime.ensure_accepting().map_err(|e| e.to_string())?;
    let message_seq = runtime
        .events()
        .append(&session.company, event.clone())
        .await
        .map_err(|e| e.to_string())?;
    // The durable half of a mention, exactly as the REST chat path files it.
    // The ACP surface is just another operator ingress: an `@user` an ACP
    // client types must badge that person the same way a console message
    // does, or the reply renders as a chip and nothing else.
    if let CompanyEvent::OperatorMessage { mentions, .. } = &event
        && !mentions.is_empty()
    {
        runtime
            .notify_mentions(&session.company, mentions, &message_seq, by.as_ref(), &chat)
            .await;
    }
    // The other half of the same resolution (B-101): every `@name` that
    // reached two things and therefore reached nobody, on the same not-fatal
    // terms as the notification above. Every ACP prompt is unrooted (this
    // session model has no thread concept), matching the `parent: None` on
    // the event just journaled above.
    runtime
        .post_mention_ambiguity_note(&chat, None, &resolved.ambiguous)
        .await;
    let report = runtime
        .run_journaled_cycle(vec![(message_seq, event)], None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(prompt_result(report))
}

/// Builds a `session/prompt` result from a finished cycle.
///
/// A park does not suspend this host's ACP turn (see `acp::approvals`'s
/// module docs) — the cycle still completes and answers `end_turn`. The
/// client learns of the park through a notification per parked approval
/// instead.
fn prompt_result(report: crate::runtime::CycleReport) -> Value {
    let mut updates = report
        .responses
        .into_iter()
        .map(|reply| {
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": reply.text }
            })
        })
        .collect::<Vec<_>>();
    for approval_id in &report.parked {
        updates.push(super::approvals::parked_update(
            approval_id.as_ref(),
            "OpenCompany parked an effect from this turn, awaiting your approval",
        ));
    }
    json!({ "stopReason": "end_turn", "updates": updates })
}

/// The text of an ACP `session/prompt`, from its content-block array.
///
/// ACP represents `params.prompt` as a `ContentBlock[]`, so a plain text
/// prompt arrives as `[{"type":"text","text":"hello"}]` — never as a `text`
/// field on the prompt itself. Only `text` blocks are consumed; every other
/// block type is rejected with its name, because this host advertises no
/// image, audio or embedded-context capability, and silently dropping a block
/// would send a turn without content the client believed it had sent.
fn prompt_text(params: &Value) -> Result<String, String> {
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| "`prompt` must be an array of content blocks".to_string())?;
    let mut text = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let value = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "a text content block must carry a `text` string".to_string())?;
                text.push_str(value);
            }
            Some(other) => {
                return Err(format!(
                    "unsupported prompt content block type `{other}`; this host accepts text \
                     blocks only"
                ));
            }
            None => {
                return Err("a prompt content block must carry a `type`".to_string());
            }
        }
    }
    if text.is_empty() {
        return Err("`prompt` carried no text".to_string());
    }
    Ok(text)
}

#[cfg(test)]
#[path = "transport_test_support.rs"]
mod test_support;
#[cfg(test)]
#[path = "transport_call_handler_tests.rs"]
mod tests_call_handler;
#[cfg(test)]
#[path = "transport_parsing_tests.rs"]
mod tests_parsing;
#[cfg(test)]
#[path = "transport_prompt_tests.rs"]
mod tests_prompt;
#[cfg(test)]
#[path = "transport_session_tests.rs"]
mod tests_session;
