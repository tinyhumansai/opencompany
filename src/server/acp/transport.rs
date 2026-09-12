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

fn target(params: &Value) -> Result<(&str, String, Option<String>), String> {
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
        .unwrap_or(crate::server::ops::language::DEFAULT_DESK)
        .to_string();
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
mod test {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::EventSeq;
    use crate::ports::types::{ApprovalId, CompressedTrace, CycleRequest, CycleResult, TokenUsage};
    use crate::ports::users::{UserRecord, UserRole, UserStatus};
    use crate::ports::{Brain, CompanyStore, CycleHost, SessionKind, SessionRecord};
    use crate::server::graphql::auth::UserPrincipal;
    use crate::server::platform_auth::PlatformClaims;
    use crate::server::users::cookie::session_cookie_name;
    use crate::server::users::token::{OsTokens, mint_session_token, sha256_hex};
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, ports::types::CompanyRecord};

    #[test]
    fn target_requires_an_explicit_company() {
        assert!(target(&json!({ "_meta": { "opencompany": {} } })).is_err());
    }

    #[test]
    fn target_defaults_to_general() {
        let (_, chat, _) =
            target(&json!({ "_meta": { "opencompany": { "company": "acme" } } })).unwrap();
        assert_eq!(chat, crate::server::ops::language::DEFAULT_DESK);
    }

    #[test]
    fn target_reads_an_agent_pin() {
        let (_, _, agent) =
            target(&json!({ "_meta": { "opencompany": { "company": "acme", "agentId": "ceo" } } }))
                .unwrap();
        assert_eq!(agent.as_deref(), Some("ceo"));
    }

    #[test]
    fn initialize_result_is_acp_shaped() {
        let result = initialize_result();
        // Numeric ACP version, not MCP's date-valued protocolVersion.
        assert_eq!(result["protocolVersion"], json!(1));
        assert!(result.get("capabilities").is_none(), "no MCP capabilities");
        assert!(result.get("serverInfo").is_none(), "no MCP serverInfo");
        // The two ACP-required result fields.
        assert!(result.get("agentCapabilities").is_some());
        assert!(result.get("agentInfo").is_some());
        assert!(result["agentInfo"]["name"].is_string());
        assert!(result["agentInfo"]["version"].is_string());
    }

    #[test]
    fn prompt_blocks_concatenate_text() {
        let params = json!({
            "prompt": [
                { "type": "text", "text": "hello " },
                { "type": "text", "text": "world" },
            ]
        });
        assert_eq!(prompt_text(&params).unwrap(), "hello world");
    }

    #[test]
    fn prompt_must_be_an_array_of_blocks() {
        assert!(prompt_text(&json!({ "prompt": "hello" })).is_err());
        assert!(prompt_text(&json!({ "prompt": { "text": "hello" } })).is_err());
    }

    #[test]
    fn unsupported_prompt_blocks_are_named() {
        let err =
            prompt_text(&json!({ "prompt": [ { "type": "image", "data": "..." } ] })).unwrap_err();
        assert!(
            err.contains("image"),
            "rejected by type, not generically: {err}"
        );
    }

    #[test]
    fn an_empty_prompt_is_refused() {
        assert!(prompt_text(&json!({ "prompt": [] })).is_err());
    }

    #[test]
    fn owner_keys_a_user_by_company_as_well_as_id() {
        // `user_id` is only guaranteed unique within a company, so two
        // companies minting the same id must not collide into one owner.
        let same_id_in_acme = GqlAuth::User(UserPrincipal {
            company: CompanyId::new("acme"),
            user_id: "u1".to_string(),
            email: "a@example.test".to_string(),
            role: UserRole::Admin,
            must_change_password: false,
            session_token_hash: "hash".to_string(),
            credential: crate::ports::SessionKind::Browser,
        });
        let same_id_in_globex = GqlAuth::User(UserPrincipal {
            company: CompanyId::new("globex"),
            user_id: "u1".to_string(),
            email: "b@example.test".to_string(),
            role: UserRole::Admin,
            must_change_password: false,
            session_token_hash: "hash".to_string(),
            credential: crate::ports::SessionKind::Browser,
        });
        assert_ne!(owner(&same_id_in_acme), owner(&same_id_in_globex));
    }

    #[test]
    fn owner_canonicalizes_the_platform_tenant() {
        // `authorize_address` compares tenants in canonical_tenant form, so a
        // raw-string owner key would treat `tenant:acme` and `acme` as two
        // different owners even though they name the same tenant.
        let prefixed = GqlAuth::Platform(PlatformClaims {
            tenant: "tenant:acme".to_string(),
            scopes: Default::default(),
            companies: None,
        });
        let bare = GqlAuth::Platform(PlatformClaims {
            tenant: "acme".to_string(),
            scopes: Default::default(),
            companies: None,
        });
        assert_eq!(owner(&prefixed), owner(&bare));
    }

    /// A brain that answers a cycle with nothing, so the ACP `prompt` turn
    /// completes without an inference credential. The notification this suite
    /// asserts on is filed before the turn runs, so the empty answer is fine.
    struct SilentBrain;

    #[async_trait]
    impl Brain for SilentBrain {
        async fn run_cycle(
            &self,
            req: CycleRequest,
            _host: &dyn CycleHost,
        ) -> crate::Result<CycleResult> {
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(req.cycle_id, "silent test brain")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    async fn seed_user(state: &AppState, company: &CompanyId, id: &str, display: &str) -> String {
        let runtime = state.registry().get(company).expect("company");
        let now = crate::ports::now_millis();
        runtime
            .users()
            .upsert_user(
                company,
                &UserRecord {
                    id: id.to_string(),
                    email: format!("{id}@example.test"),
                    display_name: Some(display.to_string()),
                    avatar: None,
                    role: UserRole::Member,
                    status: UserStatus::Active,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: now,
                    last_seen_at_millis: None,
                    updated_at_millis: now,
                },
            )
            .await
            .expect("seed_user: upsert");
        id.to_string()
    }

    /// A host whose registry runtime answers cycles with [`SilentBrain`], on
    /// the `acp,runner,tinymemory` lane — the one that executes `server::acp`.
    async fn acp_state(home: &std::path::Path) -> AppState {
        let manifest: CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "product_manager"
role = "Product Manager"

[[agent]]
id = "writer"
role = "Writer"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = []

[[group_chat]]
id = "writer"
name = "Writer desk"
members = []

[policy]
mode = "full"
"#,
        )
        .unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                overlay_desk_hive: Vec::new(),
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
        let runtime = crate::runtime::RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .with_brain(std::sync::Arc::new(SilentBrain))
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        state
    }

    /// Same as [`acp_state`], but the sole company is `[users] mode = "none"`
    /// — the packaged-desktop shape with no sign-in, reachable by anyone who
    /// can reach the loopback bind at all.
    async fn acp_state_none_mode(home: &std::path::Path) -> AppState {
        let manifest: CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "product_manager"
role = "Product Manager"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = []

[policy]
mode = "full"

[users]
mode = "none"
"#,
        )
        .unwrap();
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
                overlay_desk_hive: Vec::new(),
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
        let runtime = crate::runtime::RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .with_brain(std::sync::Arc::new(SilentBrain))
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        state
    }

    /// Builds a bare JSON-RPC `POST /acp` request with no auth headers.
    fn acp_call_request(body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/acp")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    /// An `@alice-smith` ACP prompt must badge alice exactly as a console
    /// message would: the ACP surface is just another operator ingress, and the
    /// durable notification is what lets an offline person see the mention at
    /// all.
    #[tokio::test]
    async fn a_prompt_mention_files_the_durable_notification() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-mention-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).expect("company");

        // Two people: the operator driving the prompt, and the person it names.
        let admin = seed_user(&state, &company, "u-admin", "Admin Person").await;
        let alice = seed_user(&state, &company, "u-alice", "Alice Smith").await;
        let auth = GqlAuth::User(UserPrincipal {
            company: company.clone(),
            user_id: admin,
            email: "admin@example.test".to_string(),
            role: UserRole::Admin,
            must_change_password: false,
            session_token_hash: "hash".to_string(),
            credential: crate::ports::SessionKind::Browser,
        });

        state
            .acp_sessions()
            .open(
                "conn-1",
                &owner(&auth),
                crate::server::acp::AcpSession {
                    id: "s-1".to_string(),
                    company: company.clone(),
                    chat: "engineering".to_string(),
                    agent_id: None,
                },
                crate::ports::now_millis(),
            )
            .expect("open session");

        let result = prompt(
            &state,
            &auth,
            &json!({
                "sessionId": "s-1",
                "prompt": [
                    { "type": "text", "text": "@alice-smith please review the invoice" },
                ],
                "_meta": { "opencompany/connectionId": "conn-1" },
            }),
        )
        .await;
        assert!(result.is_ok(), "prompt failed: {result:?}");

        // The durable half of the mention: the person named gets a row they can
        // badge, placed in the channel the prompt ran in.
        let rows = runtime
            .notifications()
            .list(&company, &alice)
            .await
            .expect("list");
        assert_eq!(rows.len(), 1, "an @alice prompt must badge alice");
        assert_eq!(rows[0].notification.kind, "mention");
        assert_eq!(rows[0].notification.context.as_deref(), Some("engineering"));
        // And the author is not badged for their own prompt.
        let admin_rows = runtime
            .notifications()
            .list(&company, "u-admin")
            .await
            .expect("list");
        assert!(admin_rows.is_empty(), "{admin_rows:?}");
    }

    /// B-101, on the ACP ingress (codex P2): a `@writer` that names both the
    /// `writer` teammate and the `writer` desk must be refused and reported,
    /// exactly as the REST chat path's `accept_chat_turn` does it. Before this
    /// fix the ACP `session/prompt` handler called the plain `resolve_mentions`
    /// and never posted the ambiguity note, so an ACP client (e.g. Zed) sending
    /// an ambiguous `@name` pinged nobody with no durable refusal anywhere.
    #[tokio::test]
    async fn a_prompt_ambiguous_mention_is_reported_in_the_channel() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-ambiguous-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).expect("company");
        let admin = seed_user(&state, &company, "u-admin", "Admin Person").await;
        let auth = GqlAuth::User(UserPrincipal {
            company: company.clone(),
            user_id: admin,
            email: "admin@example.test".to_string(),
            role: UserRole::Admin,
            must_change_password: false,
            session_token_hash: "hash".to_string(),
            credential: crate::ports::SessionKind::Browser,
        });

        state
            .acp_sessions()
            .open(
                "conn-1",
                &owner(&auth),
                crate::server::acp::AcpSession {
                    id: "s-1".to_string(),
                    company: company.clone(),
                    chat: "engineering".to_string(),
                    agent_id: None,
                },
                crate::ports::now_millis(),
            )
            .expect("open session");

        let result = prompt(
            &state,
            &auth,
            &json!({
                "sessionId": "s-1",
                "prompt": [
                    { "type": "text", "text": "@writer can you draft the autumn brief?" },
                ],
                "_meta": { "opencompany/connectionId": "conn-1" },
            }),
        )
        .await;
        assert!(result.is_ok(), "prompt failed: {result:?}");

        // The durable refusal notice: the same `AgentReply` the REST chat path
        // posts, attributed to the runtime and landing in the channel the
        // prompt ran in.
        let events = runtime
            .events()
            .read_from(&company, EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        let advisories: Vec<(String, String)> = events
            .into_iter()
            .filter_map(|stored| match stored.event {
                CompanyEvent::AgentReply {
                    agent_id,
                    chat_id,
                    text,
                    ..
                } if agent_id == crate::ports::SYSTEM_AUTHOR => Some((chat_id, text)),
                _ => None,
            })
            .collect();
        assert_eq!(
            advisories.len(),
            1,
            "exactly one ambiguity note, cross-ingress: {advisories:?}"
        );
        let (chat, text) = &advisories[0];
        assert_eq!(chat, "engineering");
        assert!(text.contains("@writer"), "names the literal typed: {text}");
        assert!(
            text.contains("pinged nobody"),
            "states what happened: {text}"
        );
    }

    /// A runtime being replaced refuses the prompt *before* it is journaled
    /// (codex P2): a message appended and then rejected would stay in the
    /// transcript with nothing that will ever answer it — the ordering the
    /// REST chat path holds via `accept_chat_turn`.
    #[tokio::test]
    async fn a_quiesced_runtime_refuses_prompt_before_journaling() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-quiesce-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).expect("company");
        let admin = seed_user(&state, &company, "u-admin", "Admin Person").await;
        let auth = GqlAuth::User(UserPrincipal {
            company: company.clone(),
            user_id: admin,
            email: "admin@example.test".to_string(),
            role: UserRole::Admin,
            must_change_password: false,
            session_token_hash: "hash".to_string(),
            credential: crate::ports::SessionKind::Browser,
        });

        state
            .acp_sessions()
            .open(
                "conn-1",
                &owner(&auth),
                crate::server::acp::AcpSession {
                    id: "s-1".to_string(),
                    company: company.clone(),
                    chat: "engineering".to_string(),
                    agent_id: None,
                },
                crate::ports::now_millis(),
            )
            .expect("open session");

        runtime.quiesce().await;

        let result = prompt(
            &state,
            &auth,
            &json!({
                "sessionId": "s-1",
                "prompt": [
                    { "type": "text", "text": "please review the invoice" },
                ],
                "_meta": { "opencompany/connectionId": "conn-1" },
            }),
        )
        .await;
        assert!(result.is_err(), "a quiesced runtime must refuse the prompt");

        // And nothing was journaled: the refusal happened before the append.
        let events = runtime
            .events()
            .read_from(&company, EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        assert!(
            events
                .iter()
                .all(|stored| !matches!(&stored.event, CompanyEvent::OperatorMessage { .. })),
            "a refused prompt must not leave a message in the journal: {events:?}"
        );
    }

    /// Issue #1781 review (Codex P1): `prompt` used to journal straight to
    /// `runtime.events()`, never through the REST `/chat` route's
    /// `chat_and_emit`, so a session opened with `_meta.opencompany.chat =
    /// "operator"` could post into the durable, supposedly read-only Operator
    /// system feed. `acme` (from `acp_state`) has no real `operator` desk or
    /// teammate, so this is the ordinary, non-grandfathered case REST already
    /// refuses — the ACP surface must refuse it identically.
    #[tokio::test]
    async fn an_acp_prompt_addressed_to_the_operator_channel_is_refused() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-operator-guard-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).expect("company");
        let admin = seed_user(&state, &company, "u-admin", "Admin Person").await;
        let auth = GqlAuth::User(UserPrincipal {
            company: company.clone(),
            user_id: admin,
            email: "admin@example.test".to_string(),
            role: UserRole::Admin,
            must_change_password: false,
            session_token_hash: "hash".to_string(),
            credential: crate::ports::SessionKind::Browser,
        });

        // Mirrors what `open_session` stores for an unpinned session whose
        // client requested `_meta.opencompany.chat = "operator"`
        // (`AcpSession::thread_key` passes an unpinned request through
        // verbatim).
        state
            .acp_sessions()
            .open(
                "conn-1",
                &owner(&auth),
                crate::server::acp::AcpSession {
                    id: "s-1".to_string(),
                    company: company.clone(),
                    chat: "operator".to_string(),
                    agent_id: None,
                },
                crate::ports::now_millis(),
            )
            .expect("open session");

        let result = prompt(
            &state,
            &auth,
            &json!({
                "sessionId": "s-1",
                "prompt": [
                    { "type": "text", "text": "hello from a session pinned to the feed" },
                ],
                "_meta": { "opencompany/connectionId": "conn-1" },
            }),
        )
        .await;
        assert!(
            result.is_err(),
            "a prompt addressed to the read-only Operator channel must be refused"
        );

        // And nothing was journaled: the refusal happened before the append,
        // same ordering the quiesced-runtime test above proves.
        let events = runtime
            .events()
            .read_from(&company, EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        assert!(
            events
                .iter()
                .all(|stored| !matches!(&stored.event, CompanyEvent::OperatorMessage { .. })),
            "a refused prompt must not leave a message in the journal: {events:?}"
        );
    }

    fn admin_auth(company: &CompanyId, user_id: String, session_token_hash: &str) -> GqlAuth {
        GqlAuth::User(UserPrincipal {
            company: company.clone(),
            user_id,
            email: "admin@example.test".to_string(),
            role: UserRole::Admin,
            must_change_password: false,
            session_token_hash: session_token_hash.to_string(),
            credential: crate::ports::SessionKind::Browser,
        })
    }

    /// A colon is legal in both halves of the owner key, so the join has to be
    /// injective on its own rather than by assuming the components are clean.
    #[test]
    fn two_different_principals_never_share_an_owner_key() {
        let left = owner(&admin_auth(&CompanyId::new("a:b"), "c".to_string(), "h"));
        let right = owner(&admin_auth(&CompanyId::new("a"), "b:c".to_string(), "h"));
        assert_ne!(
            left, right,
            "a company and a user id that split differently must not collide"
        );

        // The same principal still resolves to one stable key, or an operator
        // would lose their own connection between calls.
        let again = owner(&admin_auth(&CompanyId::new("a:b"), "c".to_string(), "h"));
        assert_eq!(left, again);
    }

    #[tokio::test]
    async fn a_caller_cannot_open_a_session_on_a_connection_it_does_not_own() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-conn-owner-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let alice = seed_user(&state, &company, "u-alice", "Alice").await;
        let bob = seed_user(&state, &company, "u-bob", "Bob").await;
        let auth_alice = admin_auth(&company, alice, "hash-alice");
        let auth_bob = admin_auth(&company, bob, "hash-bob");
        let params = json!({
            "_meta": {
                "opencompany": { "company": "acme" },
                "opencompany/connectionId": "conn-shared",
            }
        });

        let first = open_session(&state, &auth_alice, &params).await;
        assert!(first.is_ok(), "alice opens the connection first: {first:?}");

        let second = open_session(&state, &auth_bob, &params).await;
        assert!(
            second.is_err(),
            "bob must not be able to open a session on alice's connection"
        );

        // And bob gets no view into what alice has, by any surface.
        let listed = list_sessions(&state, &auth_bob, &params);
        assert!(listed.is_err(), "bob cannot list alice's connection either");
    }

    #[tokio::test]
    async fn open_session_refuses_once_the_per_connection_cap_is_hit() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-conn-cap-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let admin = seed_user(&state, &company, "u-admin", "Admin Person").await;
        let auth = admin_auth(&company, admin, "hash");
        let params = json!({
            "_meta": {
                "opencompany": { "company": "acme" },
                "opencompany/connectionId": "conn-cap",
            }
        });

        for _ in 0..crate::server::acp::session::MAX_SESSIONS_PER_CONNECTION {
            let result = open_session(&state, &auth, &params).await;
            assert!(result.is_ok(), "{result:?}");
        }
        let refusal = open_session(&state, &auth, &params).await;
        assert!(refusal.is_err(), "the cap must refuse the next open");
    }

    #[tokio::test]
    async fn disconnect_closes_every_session_on_the_connection_at_once() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-disconnect-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let admin = seed_user(&state, &company, "u-admin", "Admin Person").await;
        let auth = admin_auth(&company, admin, "hash");
        let params = json!({
            "_meta": {
                "opencompany": { "company": "acme" },
                "opencompany/connectionId": "conn-close",
            }
        });

        open_session(&state, &auth, &params)
            .await
            .expect("first session");
        open_session(&state, &auth, &params)
            .await
            .expect("second session");

        let closed = disconnect(&state, &auth, &params);
        assert!(closed.is_ok());

        // The whole connection is gone, not merely emptied of sessions:
        // `list_sessions` on an unknown connection is the same refusal as one
        // this caller never owned.
        assert!(
            list_sessions(&state, &auth, &params).is_err(),
            "the connection must not survive its own disconnect"
        );
    }

    #[test]
    fn a_parked_turn_carries_an_approval_notification_but_still_ends_the_turn() {
        let report = crate::runtime::CycleReport {
            cycle_id: "c1".to_string(),
            responses: Vec::new(),
            executed_effects: Vec::new(),
            parked: vec![ApprovalId::from("appr-1".to_string())],
            persisted_seq: None,
            input_seqs: Vec::new(),
        };
        let result = prompt_result(report);
        assert_eq!(result["stopReason"], "end_turn");
        let updates = result["updates"].as_array().expect("updates array");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["_meta"]["opencompany/approval"]["id"], "appr-1");
    }

    #[test]
    fn a_clean_turn_carries_no_approval_notification() {
        let report = crate::runtime::CycleReport {
            cycle_id: "c1".to_string(),
            responses: Vec::new(),
            executed_effects: Vec::new(),
            parked: Vec::new(),
            persisted_seq: None,
            input_seqs: Vec::new(),
        };
        let result = prompt_result(report);
        assert_eq!(result["stopReason"], "end_turn");
        assert!(
            result["updates"]
                .as_array()
                .expect("updates array")
                .is_empty()
        );
    }

    // -----------------------------------------------------------------
    // PLAT-057 / PLAT-060: the `call` HTTP handler itself. Every test above
    // this point calls `open_session`/`prompt`/etc. directly, bypassing the
    // axum extraction, method dispatch and JSON-RPC envelope that only `call`
    // (mounted by `router()`) actually implements.
    // -----------------------------------------------------------------

    /// PLAT-060 (AUTH): a `none`-mode company's local owner is reachable over
    /// the real HTTP `call` handler with zero credentials — no cookie, no
    /// bearer — same as every other credential-less surface that mode grants.
    #[tokio::test]
    async fn call_handler_authenticates_a_credential_less_none_mode_request() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-none-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state_none_mode(home.path()).await;
        let app = router().with_state(state);

        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let response = app.oneshot(acp_call_request(body)).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["protocolVersion"], 1);
    }

    /// PLAT-057 (AUTH): a company with real sign-in refuses an unauthenticated
    /// `call`, over HTTP — not just at the level of the extractor unit tests.
    #[tokio::test]
    async fn call_handler_refuses_an_unauthenticated_request() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-auth-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let app = router().with_state(state);

        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let response = app.oneshot(acp_call_request(body)).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// PLAT-057 (STATE): a user who must change their password is refused
    /// *before* any ACP method runs, over the real HTTP path — the same
    /// boundary `ScopedCompany` enforces for the operator API.
    #[tokio::test]
    async fn call_handler_refuses_a_temporary_password_user_before_running_any_method() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-temp-pw-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).expect("company");
        let now = crate::ports::now_millis();
        runtime
            .users()
            .upsert_user(
                &company,
                &UserRecord {
                    id: "u-temp".to_string(),
                    email: "temp@example.test".to_string(),
                    display_name: None,
                    avatar: None,
                    role: UserRole::Admin,
                    status: UserStatus::Active,
                    password_hash: None,
                    must_change_password: true,
                    created_at_millis: now,
                    last_seen_at_millis: None,
                    updated_at_millis: now,
                },
            )
            .await
            .expect("seed temp-password user");
        let token = mint_session_token(&OsTokens);
        runtime
            .sessions()
            .create(
                &company,
                &SessionRecord {
                    id: "s-temp".to_string(),
                    token_hash: sha256_hex(&token),
                    user_id: "u-temp".to_string(),
                    created_at_millis: now,
                    expires_at_millis: now + 60_000,
                    user_agent: None,
                    kind: SessionKind::Browser,
                    label: None,
                },
            )
            .await
            .expect("seed session");
        let cookie_name = session_cookie_name(&company).expect("cookie name");

        let app = router().with_state(state);
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let mut request = acp_call_request(body);
        request.headers_mut().insert(
            axum::http::header::COOKIE,
            format!("{cookie_name}={token}").parse().unwrap(),
        );
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// PLAT-057 (FAIL): an unsupported ACP method comes back as a `-32602`
    /// JSON-RPC error envelope, over the real HTTP path — not a raw error, not
    /// an HTTP-level 4xx/5xx.
    #[tokio::test]
    async fn call_handler_reports_an_unsupported_method_as_a_json_rpc_error() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-fail-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state_none_mode(home.path()).await;
        let app = router().with_state(state);

        let body = json!({
            "jsonrpc": "2.0",
            "id": "req-9",
            "method": "session/frobnicate",
            "params": {},
        });
        let response = app.oneshot(acp_call_request(body)).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["id"], "req-9");
        assert_eq!(value["error"]["code"], -32602);
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("session/frobnicate")
        );
    }

    /// PLAT-057 (BOUND): the JSON-RPC `id` round-trips exactly, including the
    /// boundary case of a request that omits it entirely (must answer `null`,
    /// not fail or invent one).
    #[tokio::test]
    async fn call_handler_echoes_the_request_id_including_when_absent() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-bound-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state_none_mode(home.path()).await;
        let app = router().with_state(state);

        let body = json!({ "jsonrpc": "2.0", "id": 42, "method": "initialize", "params": {} });
        let response = app.clone().oneshot(acp_call_request(body)).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["id"], 42);

        let body = json!({ "jsonrpc": "2.0", "method": "initialize", "params": {} });
        let response = app.oneshot(acp_call_request(body)).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["id"], Value::Null);
        assert_eq!(value["result"]["protocolVersion"], 1);
    }

    /// PLAT-057 (INPUT): a body that is not valid JSON at all must not panic
    /// or hang the handler — it is rejected before `call`'s body even runs.
    #[tokio::test]
    async fn call_handler_rejects_a_body_that_is_not_valid_json() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-input-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state_none_mode(home.path()).await;
        let app = router().with_state(state);

        let request = Request::builder()
            .method("POST")
            .uri("/acp")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(b"{ this is not json".to_vec()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();

        assert!(
            response.status().is_client_error(),
            "a malformed JSON body must be rejected, got {:?}",
            response.status()
        );
    }

    /// Mints a real, HTTP-carriable admin session for `acp_state`'s "acme"
    /// company, returning its `Cookie` header value.
    async fn seed_admin_session_cookie(
        state: &AppState,
        company: &CompanyId,
        user_id: &str,
    ) -> String {
        let runtime = state.registry().get(company).expect("company");
        let now = crate::ports::now_millis();
        runtime
            .users()
            .upsert_user(
                company,
                &UserRecord {
                    id: user_id.to_string(),
                    email: format!("{user_id}@example.test"),
                    display_name: None,
                    avatar: None,
                    role: UserRole::Admin,
                    status: UserStatus::Active,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: now,
                    last_seen_at_millis: None,
                    updated_at_millis: now,
                },
            )
            .await
            .expect("seed admin");
        let token = mint_session_token(&OsTokens);
        runtime
            .sessions()
            .create(
                company,
                &SessionRecord {
                    id: format!("s-{user_id}"),
                    token_hash: sha256_hex(&token),
                    user_id: user_id.to_string(),
                    created_at_millis: now,
                    expires_at_millis: now + 60_000,
                    user_agent: None,
                    kind: SessionKind::Browser,
                    label: None,
                },
            )
            .await
            .expect("seed session");
        let cookie_name = session_cookie_name(company).expect("cookie name");
        format!("{cookie_name}={token}")
    }

    fn session_new_request(cookie: &str, connection_id: &str, request_id: u64) -> Request<Body> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "session/new",
            "params": {
                "_meta": {
                    "opencompany": { "company": "acme" },
                    "opencompany/connectionId": connection_id,
                }
            }
        });
        let mut request = acp_call_request(body);
        request
            .headers_mut()
            .insert(axum::http::header::COOKIE, cookie.parse().unwrap());
        request
    }

    /// PLAT-057 (LIMIT): the per-connection session cap
    /// (`MAX_SESSIONS_PER_CONNECTION`) is enforced through the real HTTP `call`
    /// handler, not just the `open_session` inner function every test above
    /// this point calls directly — the handler's own JSON-RPC dispatch and
    /// envelope sit between a caller and that cap in production, and neither
    /// was ever exercised together with it.
    #[tokio::test]
    async fn call_handler_refuses_session_new_once_the_per_connection_cap_is_hit() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-limit-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let cookie = seed_admin_session_cookie(&state, &company, "u-cap-admin").await;
        let app = router().with_state(state);

        for i in 0..crate::server::acp::session::MAX_SESSIONS_PER_CONNECTION {
            let response = app
                .clone()
                .oneshot(session_new_request(&cookie, "conn-http-cap", i as u64))
                .await
                .unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(
                value.get("result").is_some(),
                "open #{i} must succeed: {value:?}"
            );
        }

        let response = app
            .oneshot(session_new_request(&cookie, "conn-http-cap", 999))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "still a JSON-RPC 200");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value.get("error").is_some(),
            "the session past the cap must come back a JSON-RPC error: {value:?}"
        );
    }

    /// PLAT-057 (CONC): two different principals racing `session/new` on the
    /// same `connectionId`, over the real HTTP handler concurrently rather than
    /// sequentially. `a_caller_cannot_open_a_session_on_a_connection_it_does_
    /// not_own` proves the inner function's ownership rule one call at a time;
    /// this proves the lock the handler sits on top of actually serializes two
    /// requests that land at the same time, rather than both racing past a
    /// check-then-insert and one silently losing its own connection.
    #[tokio::test]
    async fn call_handler_lets_only_one_of_two_concurrent_openers_win_a_connection() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-conc-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state(home.path()).await;
        let company = CompanyId::new("acme");
        let cookie_a = seed_admin_session_cookie(&state, &company, "u-conc-a").await;
        let cookie_b = seed_admin_session_cookie(&state, &company, "u-conc-b").await;
        let app = router().with_state(state);

        let request_a = session_new_request(&cookie_a, "conn-http-race", 1);
        let request_b = session_new_request(&cookie_b, "conn-http-race", 2);
        let app_a = app.clone();
        let app_b = app.clone();
        let (response_a, response_b) =
            tokio::join!(app_a.oneshot(request_a), app_b.oneshot(request_b));

        let value_a: Value = serde_json::from_slice(
            &axum::body::to_bytes(response_a.unwrap().into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let value_b: Value = serde_json::from_slice(
            &axum::body::to_bytes(response_b.unwrap().into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();

        let winners = [
            value_a.get("result").is_some(),
            value_b.get("result").is_some(),
        ]
        .into_iter()
        .filter(|ok| *ok)
        .count();
        assert_eq!(
            winners, 1,
            "exactly one of two concurrent openers may win a shared connection: \
             {value_a:?} / {value_b:?}"
        );
    }

    /// PLAT-060 (STATE): `local_owner` falls back to `registry().sole()` when
    /// `/acp` cannot name an addressed company (it has no `{id}` path param).
    /// Once a second company is registered, `sole()` no longer resolves — the
    /// credential-less request must come back unauthorized, not silently
    /// authorized against whichever company happens to be `none`-mode.
    #[tokio::test]
    async fn call_handler_none_mode_owner_is_unreachable_once_a_second_company_exists() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-state-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state_none_mode(home.path()).await;

        // Control, proven first on this exact state: with only the sole
        // `none`-mode company registered, the credential-less request
        // succeeds — the same claim `call_handler_authenticates_a_credential_
        // less_none_mode_request` makes, repeated here so the 401 asserted
        // below is shown to depend on the second company, not on some other
        // difference between the two tests' fixtures.
        let control_app = router().with_state(state.clone());
        let control_body =
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {} });
        let control_response = control_app
            .oneshot(acp_call_request(control_body))
            .await
            .unwrap();
        assert_eq!(
            control_response.status(),
            StatusCode::OK,
            "control: the sole none-mode company must still answer credential-less \
             before a second company is registered"
        );

        let manifest: CompanyManifest = toml::from_str("[company]\nname = \"Globex\"\n").unwrap();
        let store = FsCompanyStore::new(home.path().to_path_buf());
        let globex = CompanyId::new("globex");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: globex.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desk_hive: Vec::new(),
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
        let globex_runtime =
            crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
                .with_id(globex.clone())
                .with_brain(std::sync::Arc::new(SilentBrain))
                .build()
                .await
                .unwrap();
        state
            .registry()
            .insert(globex, std::sync::Arc::new(globex_runtime));

        let app = router().with_state(state);
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let response = app.oneshot(acp_call_request(body)).await.unwrap();

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "a none-mode company sharing a host with a second company must not \
             silently authorize a credential-less /acp request against either one"
        );
    }

    /// PLAT-060 (FAIL): the same `none`-mode gate `local_owner` applies
    /// everywhere else — a request carrying a forwarding header is refused
    /// outright, never degraded to a session/bearer check — proven here over
    /// the real HTTP `call` handler.
    #[tokio::test]
    async fn call_handler_none_mode_refuses_a_forwarded_request() {
        let home = tempfile::Builder::new()
            .prefix("oc-acp-call-fail2-")
            .tempdir()
            .expect("tempdir");
        let state = acp_state_none_mode(home.path()).await;
        let app = router().with_state(state);

        // Control: the identical request, minus the forwarding header,
        // succeeds on this exact app — so the refusal below is shown to
        // depend on the header, not on some other difference in the fixture.
        let control_body =
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {} });
        let control_response = app
            .clone()
            .oneshot(acp_call_request(control_body))
            .await
            .unwrap();
        assert_eq!(
            control_response.status(),
            StatusCode::OK,
            "control: the same none-mode host must answer without the forwarding header"
        );

        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let mut request = acp_call_request(body);
        request
            .headers_mut()
            .insert("x-forwarded-for", "203.0.113.5".parse().unwrap());
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "a none-mode company must refuse a forwarded request outright, not \
             treat it as its own local owner"
        );
    }

    /// PLAT-062 (STATE): a turn that both parked an approval and produced a
    /// reply still reports `end_turn` — the mixed case, not just the two
    /// single-field variations above.
    #[test]
    fn a_mixed_turn_with_a_reply_and_a_park_still_ends_the_turn() {
        let report = crate::runtime::CycleReport {
            cycle_id: "c1".to_string(),
            responses: vec![crate::ports::types::OutboundMessage {
                channel: "operator".to_string(),
                agent: None,
                text: "here's what I found".to_string(),
                steps: Vec::new(),
                reply_to: None,
                task_id: None,
                outputs: Vec::new(),
                message_id: None,
                mentions: Vec::new(),
            }],
            executed_effects: Vec::new(),
            parked: vec![ApprovalId::from("appr-1".to_string())],
            persisted_seq: None,
            input_seqs: Vec::new(),
        };
        let result = prompt_result(report);
        assert_eq!(result["stopReason"], "end_turn");
        let updates = result["updates"].as_array().expect("updates array");
        // Counting alone would pass on two chunks and no notification, which is
        // the shape this case exists to refuse.
        let approvals: Vec<&str> = updates
            .iter()
            .filter_map(|u| u["_meta"]["opencompany/approval"]["id"].as_str())
            .collect();
        assert_eq!(
            approvals,
            vec!["appr-1"],
            "the parked approval must carry its own notification: {updates:?}"
        );
        assert!(
            updates.iter().any(|u| u["content"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("here's what I found"))),
            "the reply chunk must survive alongside it: {updates:?}"
        );
        assert_eq!(updates.len(), 2, "and nothing else: {updates:?}");
    }

    /// PLAT-062 (BOUND): several parked approvals in one turn each get their
    /// own notification — none dropped, none deduplicated, at the boundary of
    /// "more than one".
    #[test]
    fn every_parked_approval_in_a_multi_park_turn_gets_its_own_notification() {
        let parked: Vec<ApprovalId> = (0..5)
            .map(|i| ApprovalId::from(format!("appr-{i}")))
            .collect();
        let report = crate::runtime::CycleReport {
            cycle_id: "c1".to_string(),
            responses: Vec::new(),
            executed_effects: Vec::new(),
            parked: parked.clone(),
            persisted_seq: None,
            input_seqs: Vec::new(),
        };
        let result = prompt_result(report);
        assert_eq!(result["stopReason"], "end_turn");
        let updates = result["updates"].as_array().expect("updates array");
        assert_eq!(updates.len(), 5);
        // Five *unique* ids is not the claim — five ids that are the ones we
        // parked is. A set of unrelated ids satisfies the former.
        let mut ids: Vec<&str> = updates
            .iter()
            .map(|u| u["_meta"]["opencompany/approval"]["id"].as_str().unwrap())
            .collect();
        ids.sort_unstable();
        let mut expected: Vec<&str> = parked.iter().map(|id| id.as_ref()).collect();
        expected.sort_unstable();
        assert_eq!(
            ids, expected,
            "every parked id must appear exactly once: {updates:?}"
        );
    }
}
