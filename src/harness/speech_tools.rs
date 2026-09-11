//! Talking as a tool call.
//!
//! # Why speaking was the one thing that was not a tool
//!
//! Every other thing an agent can do in this company is a tool: it writes a
//! ledger row with `record_entry`, opens a card with `spawn_task`, reads a
//! sibling thread with `read_thread`. Speaking was not. A turn's **return text**
//! was the message, journaled on the agent's behalf by
//! [`DeskChannel::send`](crate::runtime::channel) or by the chat route. So an
//! agent could not choose a recipient, could not say something to one teammate
//! rather than to the room, and could not decline to speak — the only way to
//! stay quiet was to return an empty string, which reads as a failed turn.
//!
//! These are the tools that close that. The names, the argument shapes and the
//! description text all come from
//! [`tinyhivemind::speech`](tinyhivemind_hive::speech), which states them once,
//! as data, and asks a host to render them verbatim. Nothing here invents a
//! contract: `interpret` reads the call, and this module does what the crate's
//! own rule says a host does — **"a tool call is a request to speak; the host
//! appends, the host decides."**
//!
//! # The names are prefixed
//!
//! The crate's names are bare (`post`, `dm`, `close`, `read`) and it explicitly
//! anticipates a namespacing host: *"an MCP server called `desk` serving `post`
//! presents it as `desk_post`, and the descriptions are written to read
//! correctly either way."* This belt is namespaced, because `read` and `post`
//! are far too generic to sit unqualified beside `read_thread`,
//! `read_ledger` and `pages_read` — a model reaching for "read" would have four
//! plausible answers and no way to pick.
//!
//! # Nothing here starts a turn
//!
//! `desk_dm` is one agent addressing another, so this is the point at which the
//! agent-to-agent edge stops being hypothetical. It stays an edge that
//! **journals a row and runs nothing**.
//!
//! That is not caution for its own sake; it is the rule
//! [`CompanyEvent::AgentReply`](crate::ports::types::CompanyEvent::AgentReply)
//! already states about its own `mentions` field — *"never consulted by
//! dispatch … an agent naming another agent draws a chip and files nothing to
//! run. The edge does not exist, which is a stronger guarantee than an edge
//! that is disabled"* — and the `mention_depth` gate beside it is the bound
//! that would apply if it ever were. A recipient hears about this row the next
//! time it takes a turn, through its own session delta
//! ([`agent_session`](crate::harness::built_in::agent_session)), which is the
//! stigmergic model the whole crate is built on and needs no dispatch edge at
//! all.
//!
//! # Off by default
//!
//! Registered only when the manifest says `[speech] enabled = true`. A company
//! that does not opt in behaves byte-for-byte as it did, and an agent that has
//! the tools but answers without calling one still has its return text
//! journaled — see [`crate::harness::built_in::speech_fallback`]. Going silent
//! because a model forgot to call a tool is not an acceptable failure mode.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};
use openhuman_core::openhuman as oh;
use tinyhivemind_hive::speech::{self, CallArguments, ToolCall, Utterance, UtteranceRejection};

use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq};

/// Say one thing to the whole channel.
pub const POST_TOOL: &str = "desk_post";
/// Say one thing to named teammates only.
pub const DM_TOOL: &str = "desk_dm";
/// Say one last thing and report the work finished.
pub const CLOSE_TOOL: &str = "desk_close";
/// Read further back than the window this turn was handed.
pub const READ_TOOL: &str = "desk_read";

/// Every tool name this belt registers, for the registrar and its tests.
pub const SPEECH_TOOLS: [&str; 4] = [POST_TOOL, DM_TOOL, CLOSE_TOOL, READ_TOOL];

/// The bare crate-side name behind one of ours.
///
/// The prefix is this host's, so it is stripped before the crate is asked —
/// `interpret` is documented to take the bare name.
fn bare(name: &str) -> &str {
    name.strip_prefix("desk_").unwrap_or(name)
}

/// The crate's own description for a tool, rendered verbatim.
///
/// Verbatim is the contract: the descriptions *are* the contract text, and they
/// are the only place a seat is told that text outside a tool call reaches
/// nobody. Falls back to a plain sentence only if the crate ever stops naming a
/// tool this belt registers, which its own tests make unlikely.
fn crate_description(name: &str) -> &'static str {
    speech::tool_specs()
        .iter()
        .find(|spec| spec.name == bare(name))
        .map(|spec| spec.description)
        .unwrap_or("Say one thing to this channel.")
}

/// What every speech tool needs: who is speaking, where, and the journal.
#[derive(Clone)]
pub struct SpeechContext {
    company: CompanyId,
    agent_id: String,
    events: Arc<dyn EventLog>,
    store: Arc<dyn crate::ports::store::CompanyStore>,
}

impl SpeechContext {
    pub fn new(
        company: CompanyId,
        agent_id: String,
        events: Arc<dyn EventLog>,
        store: Arc<dyn crate::ports::store::CompanyStore>,
    ) -> Self {
        Self {
            company,
            agent_id,
            events,
            store,
        }
    }

    /// The channel this turn is answering in.
    ///
    /// `None` is a refusal, not a wildcard — the same rule `read_thread`
    /// applies. A turn with no conversation (a dispatched card, a workflow
    /// node) has no channel to speak into, and posting into a guessed one would
    /// put a line in front of people who were not in the exchange.
    fn channel(&self) -> Option<String> {
        crate::runtime::delegation::turn_conversation()
    }

    /// Which channel a `desk_post` should land in.
    ///
    /// `None` is the channel the turn is already in — the overwhelmingly common
    /// case, and the only one before a live run showed that an agent asked to
    /// say something *somewhere else* had no way to do it and answered the
    /// person who asked instead.
    ///
    /// A named desk is resolved against
    /// [`agent_channels`](crate::server::chat_history::agent_channels) — the
    /// same function that decides which channels reach this agent's own
    /// session. That is what keeps the two honest: an agent can speak exactly
    /// where it can hear, and nowhere else.
    ///
    /// A desk it does not sit on is refused rather than widened to. Reaching
    /// another desk is a *referral* — a crossing the library already models,
    /// with its own provenance chip and its own return path — and letting
    /// `desk_post` write into a room its author is not in would put a line in
    /// front of people with no record of who let it in.
    async fn resolve_desk(&self, desk: Option<&str>, ambient: &str) -> Result<String, ToolResult> {
        let Some(desk) = desk.map(str::trim).filter(|desk| !desk.is_empty()) else {
            return Ok(ambient.to_string());
        };
        let wanted = desk.trim_start_matches('#');
        let Ok(Some(record)) = self.store.load(&self.company).await else {
            // The roster could not be read, so membership cannot be checked.
            // Falling back to the ambient channel would silently say it
            // somewhere other than asked, which is the defect this argument
            // exists to fix — so it refuses instead.
            return Err(ToolResult::error(
                "The roster could not be read, so I cannot tell whether you sit on that channel.                  Say it here instead, or try again."
                    .to_string(),
            ));
        };
        let channels = crate::server::chat_history::agent_channels(&record, &self.agent_id);
        // Codex P2: an exact `id` match is resolved first and alone — desk
        // names are not unique, so a name/label match must not outrank (or be
        // outranked by iteration order against) a desk whose `id` is exactly
        // what was asked for. Only once no id matches are we in name/label
        // territory, and there an ambiguous match — more than one channel
        // answering to the same name — is refused rather than silently
        // resolved to whichever happened to iterate first.
        let found = if let Some(channel) = channels
            .iter()
            .find(|channel| channel.id.eq_ignore_ascii_case(wanted))
        {
            Some(channel)
        } else {
            let mut by_name = channels.iter().filter(|channel| {
                channel.name.eq_ignore_ascii_case(wanted)
                    || channel.label.eq_ignore_ascii_case(wanted)
            });
            match (by_name.next(), by_name.next()) {
                (Some(only), None) => Some(only),
                (Some(_), Some(_)) => {
                    return Err(ToolResult::error(format!(
                        "More than one channel you sit on answers to `{desk}`. Use its id instead."
                    )));
                }
                (None, _) => None,
            }
        };
        match found {
            Some(channel) => Ok(channel.id.clone()),
            None => {
                let reachable = channels
                    .iter()
                    .map(|channel| channel.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(ToolResult::error(format!(
                    "You do not sit on `{desk}`, so you cannot post there. You can post in:                      {reachable}. To reach another desk, refer the work across instead."
                )))
            }
        }
    }

    /// Says one thing to the whole channel.
    ///
    /// **Does not append.** The crate's rule is that a tool call is a *request*
    /// to speak and the host appends — and the host that appends here is the
    /// reply path that has always appended, because it is the one carrying the
    /// folded steps, the live SSE frame, the resolved mentions and the
    /// board-card correlation. A tool holds none of those, so a tool that
    /// appended directly would produce a bubble poorer than the one the same
    /// sentence gets today.
    ///
    /// Falls back to appending only when there is no sink — a turn run outside
    /// the harness's tracking scope, where being heard beats being well
    /// formatted.
    async fn post_to_channel(&self, chat_id: String, text: String) -> ToolResult {
        if text.trim().is_empty() {
            return ToolResult::error(
                "A message with no text reaches nobody. Say what you mean, or call no tool at all."
                    .to_string(),
            );
        }
        if crate::runtime::delegation::collect_utterance(text.clone()) {
            crate::runtime::delegation::mark_turn_spoke();
            return ToolResult::success(
                "Said to the channel. It is journaled when this turn ends.".to_string(),
            );
        }
        self.say(chat_id, text, Vec::new()).await
    }

    /// Appends one line to the journal, with the audience the caller resolved.
    ///
    /// `audience` empty is the ordinary channel-visible case. A non-empty one is
    /// a private aside **within one desk's deliberation**: every party is on
    /// that desk, and the list narrows which of them may read the row.
    ///
    /// That is why a DM does NOT use it — see [`Self::dm`]. An `audience` is a
    /// narrowing *inside* a channel, and it cannot carry a row *across* one.
    ///
    /// This is the fallback for a post with no sink, and the append every
    /// direct row goes through.
    /// Leaves one line for each named teammate, **in that teammate's own DM
    /// channel**.
    ///
    /// # Why not the ambient channel with a narrowed audience
    ///
    /// Because that delivers nowhere, and says it delivered. It was the first
    /// shape of this method and it was wrong in a way only a live run showed:
    /// a row journaled under the *current* conversation with
    /// `audience: ["motion_designer"]` sits in the operator's DM with the
    /// speaker. Which channels reach an agent is decided by
    /// [`agent_channels`](crate::server::chat_history::agent_channels), and the
    /// speaker's own DM is not one of the recipient's — so the recipient could
    /// never read it, while the operator could. The message went to exactly the
    /// wrong person, and the tool reported success.
    ///
    /// `audience` was reached for because it needed no journal migration. It is
    /// the asides field, and an aside is a narrowing *within a desk everybody
    /// named is already on*. A DM has no such guarantee, so the narrowing has to
    /// be the channel itself.
    ///
    /// So: one row per recipient, `chat_id` set to the recipient's own DM key —
    /// the bare teammate id, which is the spelling `agent_channels` registers
    /// and the console already posts under — and an empty `audience`, because
    /// the channel has done the narrowing and a non-empty one would additionally
    /// make [`fold_asides`](crate::server::chat_history) lift the row out of the
    /// transcript as a deliberation aside, which it is not.
    ///
    /// # What it does not do
    ///
    /// Start a turn. The recipient reads this on its next turn, through its own
    /// session delta — the stigmergic model the vendored crate is built on, and
    /// the reason `AgentReply::mentions` is never consulted by dispatch. The
    /// result sentence says so rather than claiming delivery, because an agent
    /// that is told "delivered" will tell the person who asked that it was.
    async fn dm(
        &self,
        peers: Vec<String>,
        text: String,
        record: &crate::ports::types::CompanyRecord,
    ) -> ToolResult {
        if text.trim().is_empty() {
            return ToolResult::error(
                "A message with no text reaches nobody. Say what you mean, or call no tool at all."
                    .to_string(),
            );
        }
        let mut left_for: Vec<String> = Vec::new();
        // coderabbit: an earlier version returned the first journal error for
        // the whole call, even though every recipient before it had already
        // been durably appended. A retry after that error re-sent the text to
        // every one of them a second time — the exact "partial write read as
        // total failure" defect this tracks both outcomes to avoid. Every
        // recipient is now attempted regardless of an earlier one's failure,
        // and the reply says which of them actually got a row so a retry (by
        // the model, or by whoever reads the result) can address only the
        // ones that still need it.
        let mut failed_for: Vec<(String, String)> = Vec::new();
        for peer in &peers {
            // Codex P1: a desk whose id happens to equal this recipient's
            // agent id also "owns" a row journaled under the bare id
            // (`chat_history::owns` matches on the stored `chat_id` alone), so
            // every member of that desk could read a supposedly private
            // `desk_dm`. `agent_channels` already registers the `dm:<id>`
            // spelling as this teammate's own line for exactly this reason —
            // reach for it whenever the bare id collides with a desk, so the
            // row is journaled somewhere only that desk's own id would match,
            // which is far less likely to collide.
            let key = dm_journal_key(record, peer);
            let result = self.say(key, text.clone(), Vec::new()).await;
            if result.is_error {
                failed_for.push((peer.clone(), tool_result_text(&result)));
                continue;
            }
            // A DM is a hop from one openhuman session to another, and
            // this is the only place both ends are known at once. Both
            // teammates hold a live openhuman session named
            // `{company}:{agent_id}` (see `harness::session_key`); the
            // row just journaled leaves the sender's and is picked up
            // by the recipient's `prepare_delta` on its next turn.
            //
            // Logged rather than returned: the recipient's session id
            // is an internal name, and the agent has no use for it —
            // what the *operator* has a use for is being able to follow
            // one line between two sessions when a hundred of them are
            // live at once, which is exactly when the reply text alone
            // stops being enough to tell who heard what.
            tracing::debug!(
                from_session = %crate::harness::session_key::openhuman_session_key(
                    &self.company,
                    &self.agent_id,
                ),
                to_session = %crate::harness::session_key::openhuman_session_key(
                    &self.company,
                    peer,
                ),
                "[speech] dm left in the recipient's session"
            );
            left_for.push(format!("@{peer}"));
        }
        if !failed_for.is_empty() {
            let failures = failed_for
                .iter()
                .map(|(peer, error)| format!("@{peer} ({error})"))
                .collect::<Vec<_>>()
                .join("; ");
            return if left_for.is_empty() {
                ToolResult::error(format!("Could not leave it for anyone: {failures}"))
            } else {
                // Not `ToolResult::error`: some of it genuinely landed, and an
                // agent that reads "error" here and retries the whole call
                // would journal a second row for everyone already in
                // `left_for`. The text says exactly who still needs it.
                ToolResult::success(format!(
                    "Left for {}. Could not reach {} — retry `desk_dm` with only the names that \
                     failed.",
                    left_for.join(", "),
                    failures,
                ))
            };
        }
        ToolResult::success(format!(
            "Left for {}. Not delivered now — each of them reads it on their next turn, and \
             nothing here wakes them. If it needs doing rather than knowing, raise a card.",
            left_for.join(", "),
        ))
    }

    async fn say(&self, chat_id: String, text: String, audience: Vec<String>) -> ToolResult {
        if text.trim().is_empty() {
            return ToolResult::error(
                "A message with no text reaches nobody. Say what you mean, or call no tool at all."
                    .to_string(),
            );
        }
        let event = CompanyEvent::AgentReply {
            chat_id,
            agent_id: self.agent_id.clone(),
            text,
            steps: Vec::new(),
            task_id: None,
            parent: None,
            // Drawn as chips and read by nobody's dispatcher — see the module
            // docs. Left empty here rather than resolved: this belt does not
            // hold a company record at call time, and a half-resolved mention
            // is worse than none.
            mentions: Vec::new(),
            mention_depth: 0,
            audience,
        };
        match self.events.append(&self.company, event).await {
            Ok(seq) => {
                // The turn has now been heard. What it returns from here is
                // private thinking, and the return-text fallback must not
                // journal it a second time — see `delegation::TURN_SPOKE`.
                crate::runtime::delegation::mark_turn_spoke();
                ToolResult::success(format!("Said. Journaled at [{seq}]."))
            }
            Err(error) => ToolResult::error(format!("The message could not be journaled: {error}")),
        }
    }
}

/// Turns a crate-side refusal into the sentence handed back to the seat.
fn refusal(rejection: UtteranceRejection) -> ToolResult {
    ToolResult::error(rejection.to_string())
}

/// The plain-text half of a [`ToolResult`], for folding one recipient's
/// failure into another tool's own reply.
fn tool_result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            oh::tools::traits::ToolContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The journal `chat_id` a `desk_dm` to `peer` should use.
///
/// Bare, unless `peer` collides with a desk id — in which case that desk
/// would also `chat_history::owns` a row journaled under the bare spelling,
/// making a supposedly private DM readable by the whole desk. The `dm:`
/// prefix is `agent_channels`' own second spelling for this teammate's line
/// (see its doc comment), so reaching for it here does not add a channel the
/// recipient cannot already hear on.
fn dm_journal_key(record: &crate::ports::types::CompanyRecord, peer: &str) -> String {
    // Codex P1 (fresh evidence after the first collision fix): `owns` /
    // `same_conversation` match a stored row against EITHER a desk's id OR
    // its display name, so a collision on the *name* alone is exactly as
    // readable-by-the-whole-desk as a collision on the id — checking only
    // `chat.id`/`desk.id` here missed the `{ id = "triage", name = "support"
    // }` shape entirely, where a DM to agent `support` still collides.
    let collides_with_desk = record
        .manifest
        .group_chats
        .iter()
        .any(|chat| chat.id == peer || (!chat.name.trim().is_empty() && chat.name == peer))
        || record
            .overlay_desks
            .iter()
            .any(|desk| desk.id == peer || (!desk.name.trim().is_empty() && desk.name == peer));
    if collides_with_desk {
        format!("{}{peer}", crate::runtime::assignee::DM_PREFIX)
    } else {
        peer.to_string()
    }
}

/// `desk_post` — say one thing to the whole channel.
pub struct PostTool(pub SpeechContext);

#[async_trait]
impl Tool for PostTool {
    fn name(&self) -> &str {
        POST_TOOL
    }
    fn description(&self) -> &str {
        crate_description(POST_TOOL)
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "What you established, what you did not finish, and the one \
                                    teammate you need next — that teammate named first."
                },
                "desk": {
                    "type": "string",
                    "description": "Which channel to say it in. Omit for the one you are \
                                    answering in. Only a channel you sit on — to reach a desk \
                                    you are not a member of, refer the work across instead."
                }
            },
            "required": ["message"],
            "additionalProperties": false
        })
    }
    fn permission_level(&self) -> PermissionLevel {
        // Speaking is not an effect on the world outside this company, and the
        // approval bridge classifies by tool NAME rather than by this level
        // anyway (see `built_in::policy`). Declared `None` for the same reason
        // `request_approval` is: a turn that has to ask permission to answer
        // cannot answer.
        PermissionLevel::None
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(channel) = self.0.channel() else {
            return Ok(ToolResult::error(
                "`desk_post` is only available while answering in a channel; this turn is not in \
                 one."
                    .to_string(),
            ));
        };
        let message = args.get("message").and_then(Value::as_str);
        let call = speech::interpret(
            bare(POST_TOOL),
            &CallArguments {
                message,
                to: &[],
                limit: None,
            },
        );
        match call {
            Ok(ToolCall::Speak(Utterance::Post { message })) => {
                match self
                    .0
                    .resolve_desk(args.get("desk").and_then(Value::as_str), &channel)
                    .await
                {
                    Ok(target) if target == channel => {
                        // The channel this turn is already in: the ordinary
                        // path, which collects rather than appends so the reply
                        // carries its steps, its live frame and its card.
                        Ok(self.0.post_to_channel(channel, message).await)
                    }
                    // Another channel this agent sits on. It cannot ride the
                    // turn's own reply — that reply belongs to the conversation
                    // the turn is in — so it is a row of its own.
                    Ok(target) => Ok(self.0.say(target, message, Vec::new()).await),
                    Err(refusal) => Ok(refusal),
                }
            }
            Ok(_) => Ok(ToolResult::error(
                "`desk_post` says one thing to the channel; it takes no other form.".to_string(),
            )),
            Err(rejection) => Ok(refusal(rejection)),
        }
    }
}

/// `desk_dm` — say one thing to named teammates instead of the whole channel.
pub struct DmTool(pub SpeechContext);

#[async_trait]
impl Tool for DmTool {
    fn name(&self) -> &str {
        DM_TOOL
    }
    fn description(&self) -> &str {
        crate_description(DM_TOOL)
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Teammate ids, without the @."
                },
                "message": { "type": "string" }
            },
            "required": ["to", "message"],
            "additionalProperties": false
        })
    }
    fn permission_level(&self) -> PermissionLevel {
        // Speaking is not an effect on the world outside this company, and the
        // approval bridge classifies by tool NAME rather than by this level
        // anyway (see `built_in::policy`). Declared `None` for the same reason
        // `request_approval` is: a turn that has to ask permission to answer
        // cannot answer.
        PermissionLevel::None
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        // The channel's *value* is no longer used — a DM goes to the recipient's
        // own channel, not this one — but its presence still gates the tool. A
        // turn with no conversation is not a turn anybody is talking in, and
        // letting it DM would let a dispatched card message the roster.
        let Some(_channel) = self.0.channel() else {
            return Ok(ToolResult::error(
                "`desk_dm` is only available while answering in a channel; this turn is not in one."
                    .to_string(),
            ));
        };
        let to: Vec<String> = args
            .get("to")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|id| id.trim().trim_start_matches('@').to_string())
                    .filter(|id| !id.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let message = args.get("message").and_then(Value::as_str);
        let call = speech::interpret(
            bare(DM_TOOL),
            &CallArguments {
                message,
                to: &to,
                limit: None,
            },
        );
        match call {
            Ok(ToolCall::Speak(Utterance::Dm { to, message })) => {
                // Self-addressing is refused by the crate in three places and is
                // refused here too, at the one point that holds the speaker's
                // own id: a message to yourself reaches nobody else, and a row
                // whose audience is only its author is a covert channel with a
                // journal entry.
                let mut peers: Vec<String> = to
                    .iter()
                    .filter(|id| *id != &self.0.agent_id)
                    .cloned()
                    .collect();
                if peers.is_empty() {
                    return Ok(ToolResult::error(
                        "`to` names only you; a message to yourself reaches nobody else. Use \
                         `desk_post` to say it to the channel."
                            .to_string(),
                    ));
                }
                // Every named teammate must resolve to exactly one roster id.
                // `dm` below hands `peer` straight to `openhuman_session_key`,
                // and `agent_channels` registers a recipient's session under
                // its *canonical* id — so a `to` entry that was typed as a
                // display name must be replaced with that id before it ever
                // reaches `dm`, or the row is journaled under a session
                // nothing reads. `Unknown` names nobody; `Ambiguous` names more
                // than one teammate and must not silently pick either.
                let Ok(Some(record)) = self.0.store.load(&self.0.company).await else {
                    // tinysweeper: the sibling `resolve_desk` treats an
                    // unreadable roster as fatal rather than silently skipping
                    // its own membership check (same rationale, quoted there:
                    // falling through would risk exactly the row-nobody-reads
                    // outcome this check exists to prevent). A store error or
                    // `Ok(None)` here must refuse for the same reason, not
                    // fall through to `dm` with unresolved names.
                    return Ok(ToolResult::error(
                        "The roster could not be read, so I cannot tell who `to` names. Try \
                         again."
                            .to_string(),
                    ));
                };
                let mut canonical: Vec<String> = Vec::with_capacity(peers.len());
                let mut unknown: Vec<String> = Vec::new();
                let mut ambiguous: Vec<String> = Vec::new();
                for id in &peers {
                    match record.resolve_teammate_key(id) {
                        crate::ports::types::TeammateResolution::Agent(canonical_id) => {
                            canonical.push(canonical_id)
                        }
                        crate::ports::types::TeammateResolution::Unknown => {
                            unknown.push(id.clone())
                        }
                        crate::ports::types::TeammateResolution::Ambiguous(_) => {
                            ambiguous.push(id.clone())
                        }
                    }
                }
                if !unknown.is_empty() {
                    let names = unknown
                        .iter()
                        .map(|id| format!("@{id}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok(ToolResult::error(format!(
                        "Nobody on this company is called {names}. Check the roster and try \
                         again."
                    )));
                }
                if !ambiguous.is_empty() {
                    let names = ambiguous
                        .iter()
                        .map(|id| format!("@{id}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok(ToolResult::error(format!(
                        "More than one teammate answers to {names}. Use their id instead of a \
                         display name."
                    )));
                }
                peers = canonical;
                // Codex P2: canonicalization can turn a survivor of the
                // filter above back into the caller's own id — an overlay
                // teammate addressing itself by its unique DISPLAY NAME
                // (`to: ["Nova"]`, agent id `nova`) passes the raw-id
                // filter, since `"Nova" != "nova"`, and only becomes a
                // self-reference once resolved. Re-apply the same refusal
                // now that every surviving entry is a canonical id.
                peers.retain(|id| id != &self.0.agent_id);
                // Codex P2: two `to` entries can canonicalize to the same id —
                // a repeated raw id, or one spelled once by id and once by
                // display name — and without this, `dm` below loops over the
                // vector and journals the identical message once per surviving
                // entry, doubling it in the recipient's channel. Deduplicated
                // after canonicalization (not before, where the entries are
                // not yet comparable) and order-preserving, so the "Left for"
                // sentence still lists each recipient in the order they were
                // named.
                let mut seen_peers = std::collections::HashSet::with_capacity(peers.len());
                peers.retain(|id| seen_peers.insert(id.clone()));
                if peers.is_empty() {
                    return Ok(ToolResult::error(
                        "`to` names only you; a message to yourself reaches nobody else. Use \
                         `desk_post` to say it to the channel."
                            .to_string(),
                    ));
                }
                Ok(self.0.dm(peers, message, &record).await)
            }
            Ok(_) => Ok(ToolResult::error(
                "`desk_dm` says one thing to named teammates; it takes no other form.".to_string(),
            )),
            Err(rejection) => Ok(refusal(rejection)),
        }
    }
}

/// `desk_close` — say one last thing and report the work finished.
pub struct CloseTool(pub SpeechContext);

#[async_trait]
impl Tool for CloseTool {
    fn name(&self) -> &str {
        CLOSE_TOOL
    }
    fn description(&self) -> &str {
        crate_description(CLOSE_TOOL)
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The result, and why nothing is left open."
                }
            },
            "required": ["message"],
            "additionalProperties": false
        })
    }
    fn permission_level(&self) -> PermissionLevel {
        // Speaking is not an effect on the world outside this company, and the
        // approval bridge classifies by tool NAME rather than by this level
        // anyway (see `built_in::policy`). Declared `None` for the same reason
        // `request_approval` is: a turn that has to ask permission to answer
        // cannot answer.
        PermissionLevel::None
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(channel) = self.0.channel() else {
            return Ok(ToolResult::error(
                "`desk_close` is only available while answering in a channel; this turn is not in \
                 one."
                    .to_string(),
            ));
        };
        let message = args.get("message").and_then(Value::as_str);
        let call = speech::interpret(
            bare(CLOSE_TOOL),
            &CallArguments {
                message,
                to: &[],
                limit: None,
            },
        );
        match call {
            Ok(ToolCall::Speak(Utterance::Close { message })) => {
                Ok(self.0.post_to_channel(channel, message).await)
            }
            Ok(_) => Ok(ToolResult::error(
                "`desk_close` says one last thing and reports the work finished; it takes no other \
                 form."
                    .to_string(),
            )),
            Err(rejection) => Ok(refusal(rejection)),
        }
    }
}

/// `desk_read` — read further back in this channel than the turn was handed.
pub struct ReadTool(pub SpeechContext);

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        READ_TOOL
    }
    fn description(&self) -> &str {
        crate_description(READ_TOOL)
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": speech::READ_MAX,
                    "description": format!(
                        "How many recent messages to return. Default {}, max {}.",
                        speech::READ_DEFAULT, speech::READ_MAX
                    )
                }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(channel) = self.0.channel() else {
            return Ok(ToolResult::error(
                "`desk_read` is only available while answering in a channel; this turn is not in \
                 one."
                    .to_string(),
            ));
        };
        let call = speech::interpret(
            bare(READ_TOOL),
            &CallArguments {
                message: None,
                to: &[],
                limit: args.get("limit").and_then(Value::as_u64),
            },
        );
        // The clamp is the crate's, so two hosts cannot disagree about it and a
        // seat asking for the whole transcript gets a bounded answer rather
        // than its own context window back.
        let limit = match call {
            Ok(ToolCall::Read { limit }) => limit,
            Ok(_) => speech::READ_DEFAULT,
            Err(rejection) => return Ok(refusal(rejection)),
        };

        // Codex P1: `resolve_seed_desk` is an ALIAS resolver — built for a
        // human-typed or short-form channel key, and it will happily widen a
        // bare DM key that collides with a desk's display name (agent
        // `support`, desk `{ id: "triage", name: "support" }`) to that OTHER
        // desk's canonical id. `channel` here is not typed input: it is the
        // ambient conversation this turn is already bound to, the exact
        // string `post_to_channel`/`say` use verbatim as `chat_id` for this
        // turn's own posts (see `post_to_channel` above). Scanning under that
        // same literal string — not a resolved alias — is what keeps
        // `desk_read` scoped to "this channel", channel it was actually
        // authorized for, precisely as the tool's contract promises.
        let (desk_id, desk_name) = (channel.clone(), channel.clone());

        let mut lines: Vec<String> = Vec::new();
        let mut cursor: Option<EventSeq> = None;
        let mut scanned = 0usize;
        // Bounded for the reason every read in this area is: a read is a recent
        // window, and hunting the whole company journal for one is a defect
        // rather than thoroughness.
        const SEARCH_PAGE: usize = 256;
        const SEARCH_BUDGET: usize = 2048;
        while lines.len() < limit && scanned < SEARCH_BUDGET {
            let page = match self
                .0
                .events
                .read_before(&self.0.company, cursor, SEARCH_PAGE)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    return Ok(ToolResult::error(format!(
                        "This channel could not be read: {error}"
                    )));
                }
            };
            if page.is_empty() {
                break;
            }
            scanned += page.len();
            cursor = page.last().map(|event| event.seq);
            for stored in page {
                if lines.len() >= limit {
                    break;
                }
                if !crate::server::chat_history::owns(&desk_id, &desk_name, &stored.event) {
                    continue;
                }
                // The same audience narrowing the session applies: a private
                // exchange this agent is not party to is not readable by asking
                // for more of the channel.
                let line = match &stored.event {
                    CompanyEvent::AgentReply {
                        agent_id,
                        text,
                        audience,
                        ..
                    } => {
                        if !audience.is_empty()
                            && agent_id != &self.0.agent_id
                            && !audience.iter().any(|member| member == &self.0.agent_id)
                        {
                            continue;
                        }
                        format!("[{}] {agent_id}: {text}", stored.seq)
                    }
                    CompanyEvent::OperatorMessage { text, .. } => {
                        format!("[{}] operator: {text}", stored.seq)
                    }
                    _ => continue,
                };
                lines.push(line);
            }
        }
        lines.reverse();
        if lines.is_empty() {
            return Ok(ToolResult::success(
                "Nothing has been said in this channel yet.".to_string(),
            ));
        }
        // A read that was cut says so — `query_company` is the cautionary case
        // this repo already names: a partial list that reads as complete
        // becomes "we have no record of that". tinysweeper: `lines.len() <
        // limit` alone missed the OTHER way a read is cut short — the scan
        // budget running out first. A matching message can exist just past
        // `SEARCH_BUDGET`; without this, that reply reads as "nothing more to
        // see" when the honest answer is "did not look far enough".
        let truncated = lines.len() >= limit || scanned >= SEARCH_BUDGET;
        let mut body = lines.join("\n");
        if truncated {
            body.push_str(&format!(
                "\n\n(Showing the most recent {limit}. Older messages are not in this reply.)"
            ));
        }
        Ok(ToolResult::success(body))
    }
}

/// The prompt brief for a company that speaks by calling a tool.
///
/// # A tool granted, unmentioned and never called
///
/// That is the failure this repo has already named twice — `shell` wired since
/// Cell A and described in no brief, `ledger` granted and never used — and
/// speech is the worst case of it, because the fallback is *silent*. An agent
/// that never learns about `desk_post` simply answers in text, the reply path
/// journals it, and nothing anywhere reports that the feature did nothing. It
/// was observed doing exactly that on the first live run of this change.
///
/// The tool descriptions cannot carry this on their own. They say "this is the
/// only way to speak", but they say it from inside a list of forty tools, and a
/// system prompt that never mentions speaking at all outranks them.
pub fn speech_brief() -> String {
    format!(
        "\n\n## Saying things\n\
         This company talks by calling a tool. Text you write outside a tool call is your own \
         thinking and reaches nobody — it is not sent, and nobody sees it.\n\
         - `{POST_TOOL}` — say one thing to a channel. Call it exactly once, at the end of your \
         turn. This is how you answer. It says it in the channel you are answering in unless you \
         pass `desk`, which may name any channel you sit on.\n\
         - `{DM_TOOL}` — leave one thing for named teammates instead of the whole channel, when \
         you need to settle something without spending the room's attention. It **leaves** the \
         message: each of them reads it on their next turn, and nothing wakes them. Do not tell \
         anybody it was delivered, because it was not. If it needs doing rather than knowing, \
         raise a card.\n\
         - `{CLOSE_TOOL}` — say one last thing AND report the work finished. Only when it \
         genuinely is: a result somebody still has to check is not finished.\n\
         - `{READ_TOOL}` — read further back in this channel than you were handed.\n"
    )
}

/// Every speech tool, built for one agent.
pub fn speech_belt(context: SpeechContext) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(PostTool(context.clone())),
        Box::new(DmTool(context.clone())),
        Box::new(CloseTool(context.clone())),
        Box::new(ReadTool(context)),
    ]
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::StoredEvent;
    use futures::stream::{self, BoxStream};
    use std::sync::Mutex;

    /// A log that records what was appended, so a test can ask what actually
    /// reached the journal rather than what the tool said it did.
    struct RecordingLog(Mutex<Vec<CompanyEvent>>);

    /// Like [`RecordingLog`], but refuses to append to one named channel —
    /// for proving a `desk_dm` to several recipients does not treat one
    /// recipient's journal failure as a reason to report the whole call
    /// failed after earlier recipients already got a durable row.
    struct FlakyLog {
        events: Mutex<Vec<CompanyEvent>>,
        refuses: &'static str,
    }

    #[async_trait]
    impl EventLog for FlakyLog {
        async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> crate::Result<EventSeq> {
            if let CompanyEvent::AgentReply { chat_id, .. } = &event
                && chat_id == self.refuses
            {
                return Err(crate::error::OpenCompanyError::Conflict(format!(
                    "journal unavailable for {chat_id}"
                )));
            }
            let mut appended = self.events.lock().expect("test log lock");
            appended.push(event);
            Ok(EventSeq::new(appended.len() as u64))
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            Ok(Vec::new())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    /// A journal that actually answers `read_before`, for exercising
    /// `desk_read`'s scan — `RecordingLog` and `FlakyLog` both hardcode
    /// `read_from` to `Ok(Vec::new())`, which is fine for the tools that only
    /// append, but makes them useless for testing a tool whose entire job is
    /// reading history back.
    #[derive(Default)]
    struct HistoryLog(Mutex<Vec<StoredEvent>>);

    #[async_trait]
    impl EventLog for HistoryLog {
        async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> crate::Result<EventSeq> {
            let mut rows = self.0.lock().expect("test log lock");
            let seq = EventSeq::new(rows.len() as u64 + 1);
            rows.push(StoredEvent {
                seq,
                company: CompanyId::new("acme"),
                event,
                at_millis: 0,
            });
            Ok(seq)
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            seq: EventSeq,
            limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            Ok(self
                .0
                .lock()
                .expect("test log lock")
                .iter()
                .filter(|stored| stored.seq.value() >= seq.value())
                .take(limit)
                .cloned()
                .collect())
        }
        async fn read_before(
            &self,
            _id: &CompanyId,
            before: Option<EventSeq>,
            limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            let mut rows: Vec<StoredEvent> = self
                .0
                .lock()
                .expect("test log lock")
                .iter()
                .filter(|stored| before.is_none_or(|cursor| stored.seq.value() < cursor.value()))
                .cloned()
                .collect();
            rows.reverse();
            rows.truncate(limit);
            Ok(rows)
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    #[async_trait]
    impl EventLog for RecordingLog {
        async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> crate::Result<EventSeq> {
            let mut appended = self.0.lock().expect("test log lock");
            appended.push(event);
            Ok(EventSeq::new(appended.len() as u64))
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            Ok(Vec::new())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    fn context() -> (SpeechContext, Arc<RecordingLog>, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("speech-tools-")
            .tempdir()
            .expect("tempdir");
        let store: Arc<dyn crate::ports::store::CompanyStore> =
            Arc::new(crate::store::FsCompanyStore::new(dir.path()));
        let events = Arc::new(RecordingLog(Mutex::new(Vec::new())));
        let context = SpeechContext::new(
            CompanyId::new("acme"),
            "designer".to_string(),
            events.clone() as Arc<dyn EventLog>,
            store,
        );
        (context, events, dir)
    }

    /// A roster with one desk `designer` sits on and one it does not.
    async fn context_with_desks() -> (SpeechContext, Arc<RecordingLog>, tempfile::TempDir) {
        let (context, events, dir) = context();
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "designer"
role = "Designer"
description = "Draws things."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds things."

[[group_chat]]
id = "brand"
name = "Brand"
members = ["designer"]

[[group_chat]]
id = "platform"
name = "Platform"
members = ["engineer"]
"#,
        )
        .expect("valid manifest");
        let record = crate::ports::types::CompanyRecord {
            id: context.company.clone(),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_tool_grants: None,
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        };
        context.store.save(&record).await.expect("the record saves");
        (context, events, dir)
    }

    /// A roster with two operator-added teammates: `nova`, whose display name
    /// `Nova` is unique, and two sharing the display name `Rivers` — the two
    /// [`crate::ports::types::TeammateResolution`] arms `desk_dm` must not
    /// collapse into "found something, ship it".
    async fn context_with_overlay_teammates()
    -> (SpeechContext, Arc<RecordingLog>, tempfile::TempDir) {
        let (context, events, dir) = context();
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "designer"
role = "Designer"
description = "Draws things."

[[agent]]
id = "copy"
role = "Copywriter"
description = "Writes things."

[[agent]]
id = "researcher"
role = "Researcher"
description = "Finds things."
"#,
        )
        .expect("valid manifest");
        let mut record = crate::ports::types::CompanyRecord {
            id: context.company.clone(),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_tool_grants: None,
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        };
        for (id, name) in [
            ("nova", "Nova"),
            ("rivers-1", "Rivers"),
            ("rivers-2", "Rivers"),
        ] {
            record
                .overlay_agents
                .push(crate::ports::types::OverlayAgent {
                    id: id.to_string(),
                    name: name.to_string(),
                    role: "Growth".to_string(),
                    description: None,
                    tools: None,
                    model: None,
                    harness: None,
                });
        }
        context.store.save(&record).await.expect("the record saves");
        (context, events, dir)
    }

    /// `desk_dm` must resolve a display name to the roster's canonical id
    /// before journaling: `dm` names the recipient's session with it
    /// (`openhuman_session_key`), and `agent_channels` registers that session
    /// under the canonical id, never the typed name. Left unresolved, the row
    /// would be journaled under a session nothing reads (Codex P2).
    #[tokio::test]
    async fn a_dm_to_a_display_name_resolves_to_the_canonical_id() {
        let (context, events, _dir) = context_with_overlay_teammates().await;
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("designer".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["Nova"],
                    "message": "ship it"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let appended = events.0.lock().expect("lock");
        assert_eq!(appended.len(), 1, "{appended:?}");
        let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
            panic!("expected an AgentReply, got {:?}", appended[0]);
        };
        assert_eq!(
            chat_id, "nova",
            "the row must be journaled under the canonical id, not the typed display name"
        );
    }

    /// A display name two teammates share must be refused, not silently
    /// resolved to whichever one the roster happens to list first — that would
    /// journal a row under a session the operator never meant to address.
    #[tokio::test]
    async fn a_dm_to_an_ambiguous_display_name_is_refused() {
        let (context, events, _dir) = context_with_overlay_teammates().await;
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken, async {
            crate::runtime::delegation::with_turn_conversation(
                Some("designer".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["Rivers"],
                    "message": "ship it"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(result.is_error, "{result:?}");
        let said = format!("{result:?}");
        assert!(
            said.contains("more than one") || said.contains("Rivers"),
            "the refusal should name the collision: {said}"
        );
        assert!(
            events.0.lock().expect("lock").is_empty(),
            "an ambiguous name must journal nothing"
        );
    }

    /// Codex P2: canonicalizing `to` can turn a survivor of the raw self-id
    /// filter back into the caller's own id. `nova` addressing itself as
    /// `"Nova"` passes the first filter (`"Nova" != "nova"`) and must still be
    /// refused once resolution reveals it is the caller.
    #[tokio::test]
    async fn a_dm_to_your_own_display_name_is_refused() {
        let (mut context, events, _dir) = context_with_overlay_teammates().await;
        context.agent_id = "nova".to_string();
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken, async {
            crate::runtime::delegation::with_turn_conversation(
                Some("designer".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["Nova"],
                    "message": "note to self"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(result.is_error, "{result:?}");
        assert!(
            events.0.lock().expect("lock").is_empty(),
            "a self-DM by display name must journal nothing"
        );
    }

    /// tinysweeper: an unreadable roster must refuse a DM, not silently skip
    /// the check and journal a row to whatever the caller typed — the same
    /// rule `resolve_desk` already applies to a channel name.
    #[tokio::test]
    async fn a_dm_is_refused_when_the_roster_cannot_be_read() {
        let (context, events, _dir) = context();
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken, async {
            crate::runtime::delegation::with_turn_conversation(
                Some("brand".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["copy"],
                    "message": "ship it"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(result.is_error, "{result:?}");
        assert!(
            events.0.lock().expect("lock").is_empty(),
            "an unreadable roster must journal nothing"
        );
    }

    /// `desk_post` can name a channel this agent sits on, and the line lands
    /// there rather than in the conversation the turn is in.
    ///
    /// This is the capability a live run found missing: asked to say something
    /// on another desk, the agent had no argument for it and answered the person
    /// who asked instead — which reads as the message having been passed on.
    #[tokio::test]
    async fn a_post_can_name_another_channel_this_agent_sits_on() {
        let (context, events, _dir) = context_with_desks().await;
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("designer".to_string()),
                PostTool(context).execute(serde_json::json!({
                    "desk": "brand",
                    "message": "moving the hero to the left rail"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let appended = events.0.lock().expect("lock");
        let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
            panic!("expected an AgentReply, got {:?}", appended[0]);
        };
        assert_eq!(
            chat_id, "brand",
            "a named desk is where it goes; `designer` is the DM the turn was in"
        );
    }

    /// A desk this agent is not a member of is refused, and the refusal says
    /// which channels it can reach.
    ///
    /// Widening to it would put a line in front of a room with no record of who
    /// let it in. Reaching another desk is a referral — a crossing the library
    /// already models, with its own provenance chip and return path.
    #[tokio::test]
    async fn a_post_to_a_desk_this_agent_is_not_on_is_refused() {
        let (context, events, _dir) = context_with_desks().await;
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("designer".to_string()),
                PostTool(context).execute(serde_json::json!({
                    "desk": "platform",
                    "message": "you should refactor this"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(result.is_error, "{result:?}");
        let said = format!("{result:?}");
        assert!(said.contains("do not sit on"), "{said}");
        assert!(
            said.contains("Brand"),
            "the refusal names what it can reach: {said}"
        );
        assert!(
            events.0.lock().expect("lock").is_empty(),
            "a refused post journals nothing"
        );
    }

    /// The contract text is the crate's, not this host's. It is the only place a
    /// seat is told that text outside a tool call reaches nobody, so a host that
    /// paraphrased it would be quietly rewriting the rule.
    #[test]
    fn the_descriptions_are_the_crates_own() {
        for name in SPEECH_TOOLS {
            let ours = crate_description(name);
            let theirs = speech::tool_specs()
                .iter()
                .find(|spec| spec.name == bare(name))
                .expect("every registered tool is one the crate names")
                .description;
            assert_eq!(ours, theirs, "{name} paraphrased the crate");
        }
    }

    /// A post is a *request* to speak: it does not append, it is collected and
    /// the reply path appends it — with the steps, the live frame and the
    /// mentions a tool does not hold.
    #[tokio::test]
    async fn a_post_is_collected_rather_than_journaled() {
        let (context, events, _dir) = context();
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("brand".to_string()),
                PostTool(context).execute(serde_json::json!({ "message": "warmer" })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        assert_eq!(spoken.utterances(), vec!["warmer".to_string()]);
        assert!(spoken.spoke());
        assert!(
            events.0.lock().expect("lock").is_empty(),
            "a post must not append; the reply path does"
        );
    }

    /// A DM lands in the **recipient's** channel, not the speaker's.
    ///
    /// This test exists because the first version of `desk_dm` did the opposite
    /// and a live run caught it: the row went to the ambient channel with
    /// `audience: [recipient]`, which put a message meant for a teammate into
    /// the *operator's* DM with the speaker. `agent_channels` never gives the
    /// recipient the speaker's own DM, so the one person named could not read
    /// it and the one person not named could — and the tool said "Said."
    ///
    /// So the assertions are about `chat_id`, and the empty `audience` is as
    /// load-bearing as the id: a non-empty one would make `fold_asides` lift the
    /// row out of the transcript as a desk deliberation aside, which it is not.
    #[tokio::test]
    async fn a_dm_lands_in_the_recipients_own_channel() {
        let (context, events, _dir) = context_with_overlay_teammates().await;
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("brand".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["copy"],
                    "message": "between us: ship it"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let appended = events.0.lock().expect("lock");
        assert_eq!(appended.len(), 1, "{appended:?}");
        let CompanyEvent::AgentReply {
            audience,
            agent_id,
            chat_id,
            ..
        } = &appended[0]
        else {
            panic!("expected an AgentReply, got {:?}", appended[0]);
        };
        assert_eq!(agent_id, "designer");
        assert_eq!(
            chat_id, "copy",
            "a DM belongs in the recipient's channel; `brand` here is the speaker's own, which \
             the recipient cannot read"
        );
        assert!(
            audience.is_empty(),
            "the channel is the narrowing; an audience would make this a deliberation aside"
        );
    }

    /// Codex P1: when the recipient's bare agent id collides with a desk id,
    /// `chat_history::owns` would let every member of that desk read the
    /// row, so the DM must land under the `dm:`-prefixed spelling instead —
    /// the same spelling `agent_channels` already registers as this
    /// teammate's own line.
    #[tokio::test]
    async fn a_dm_to_a_peer_whose_id_collides_with_a_desk_lands_under_the_prefixed_key() {
        let (context, events, _dir) = context();
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "designer"
role = "Designer"
description = "Draws things."

[[agent]]
id = "platform"
role = "Platform engineer"
description = "Builds things."

[[group_chat]]
id = "platform"
name = "Platform"
members = ["designer"]
"#,
        )
        .expect("valid manifest");
        let record = crate::ports::types::CompanyRecord {
            id: context.company.clone(),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_tool_grants: None,
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        };
        context.store.save(&record).await.expect("the record saves");
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("platform".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["platform"],
                    "message": "between us: ship it"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let appended = events.0.lock().expect("lock");
        assert_eq!(appended.len(), 1, "{appended:?}");
        let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
            panic!("expected an AgentReply, got {:?}", appended[0]);
        };
        assert_eq!(
            chat_id, "dm:platform",
            "the bare id collides with the `platform` desk, so every desk member \
             could read a bare-keyed row; the prefixed key keeps it private"
        );
    }

    /// Codex P1 (fresh evidence): a desk's *display name*, not just its id,
    /// collides the same way — `chat_history::owns` matches on either — so a
    /// desk `{ id = "triage", name = "support" }` makes a DM to agent
    /// `support` exactly as readable-by-the-whole-desk as an id collision
    /// would.
    #[tokio::test]
    async fn a_dm_to_a_peer_whose_id_collides_with_a_desk_name_lands_under_the_prefixed_key() {
        let (context, events, _dir) = context();
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "designer"
role = "Designer"
description = "Draws things."

[[agent]]
id = "support"
role = "Support"
description = "Answers things."

[[group_chat]]
id = "triage"
name = "support"
members = ["designer"]
"#,
        )
        .expect("valid manifest");
        let record = crate::ports::types::CompanyRecord {
            id: context.company.clone(),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_tool_grants: None,
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        };
        context.store.save(&record).await.expect("the record saves");
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("triage".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["support"],
                    "message": "between us: ship it"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let appended = events.0.lock().expect("lock");
        assert_eq!(appended.len(), 1, "{appended:?}");
        let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
            panic!("expected an AgentReply, got {:?}", appended[0]);
        };
        assert_eq!(
            chat_id, "dm:support",
            "the bare id collides with the `triage` desk's display NAME (\"support\"), \
             which `owns` matches exactly like an id collision; the prefixed key keeps \
             it private"
        );
    }

    /// Two recipients get two rows, one in each of their channels.
    ///
    /// Not one row with both in an audience: there is no channel both of them
    /// read, and inventing one would be inventing a group nobody created.
    #[tokio::test]
    async fn a_dm_to_two_teammates_leaves_a_row_in_each_of_their_channels() {
        let (context, events, _dir) = context_with_overlay_teammates().await;
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("brand".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["copy", "researcher"],
                    "message": "both of you: ship it"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let appended = events.0.lock().expect("lock");
        let channels: Vec<&str> = appended
            .iter()
            .map(|event| match event {
                CompanyEvent::AgentReply { chat_id, .. } => chat_id.as_str(),
                other => panic!("expected an AgentReply, got {other:?}"),
            })
            .collect();
        assert_eq!(channels, vec!["copy", "researcher"]);
    }

    /// Codex P2: `to` naming the same recipient twice — by a repeated raw id,
    /// or (elsewhere) once by id and once by a display name that resolves to
    /// the same canonical id — must not double the row in their channel.
    #[tokio::test]
    async fn a_repeated_recipient_leaves_only_one_row() {
        let (context, events, _dir) = context_with_overlay_teammates().await;
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("brand".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["copy", "copy"],
                    "message": "said once, meant once"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let appended = events.0.lock().expect("lock");
        assert_eq!(
            appended.len(),
            1,
            "a repeated `to` entry must not journal a second row: {appended:?}"
        );
    }

    /// coderabbit: a journal failure for one recipient must not read as a
    /// failure for all of them — the recipients ahead of the failing one
    /// already have a durable row, and reporting a flat error invites a retry
    /// that journals a second one for each. `researcher`'s append is made to
    /// fail; `copy`'s must still land, and the result must say so by name
    /// rather than simply being `is_error`.
    #[tokio::test]
    async fn a_dm_failure_for_one_recipient_does_not_undo_or_hide_an_earlier_success() {
        let dir = tempfile::Builder::new()
            .prefix("speech-tools-")
            .tempdir()
            .expect("tempdir");
        let store: Arc<dyn crate::ports::store::CompanyStore> =
            Arc::new(crate::store::FsCompanyStore::new(dir.path()));
        let company = CompanyId::new("acme");
        let events = Arc::new(FlakyLog {
            events: Mutex::new(Vec::new()),
            refuses: "researcher",
        });
        let context = SpeechContext::new(
            company.clone(),
            "designer".to_string(),
            events.clone() as Arc<dyn EventLog>,
            store.clone(),
        );
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "designer"
role = "Designer"
description = "Draws things."

[[agent]]
id = "copy"
role = "Copywriter"
description = "Writes things."

[[agent]]
id = "researcher"
role = "Researcher"
description = "Finds things."

[[group_chat]]
id = "brand"
name = "Brand"
members = ["designer", "copy", "researcher"]
"#,
        )
        .expect("valid manifest");
        let record = crate::ports::types::CompanyRecord {
            id: company,
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_tool_grants: None,
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        };
        store.save(&record).await.expect("the record saves");
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("brand".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["copy", "researcher"],
                    "message": "both of you: ship it"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        let appended = events.events.lock().expect("lock");
        assert_eq!(
            appended.len(),
            1,
            "copy's row must still be journaled despite researcher's failure: {appended:?}"
        );
        let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
            panic!("expected an AgentReply, got {:?}", appended[0]);
        };
        assert_eq!(chat_id, "copy");
        let text = tool_result_text(&result);
        assert!(
            text.contains("copy"),
            "the reply must name who it did reach: {text}"
        );
        assert!(
            text.contains("researcher"),
            "the reply must name who it did not reach, so a retry can target just them: {text}"
        );
    }

    /// The result sentence does not claim delivery.
    ///
    /// Nothing here wakes the recipient — `AgentReply::mentions` is never
    /// consulted by dispatch, which is the mention-loop fuse — so the row waits
    /// until their next turn. A live run showed why the wording matters: told
    /// "Said. Journaled at [67]", the agent reported to the person who asked
    /// that the message had been "passed to them directly (delivered)". An
    /// agent repeats what its tools tell it.
    #[tokio::test]
    async fn a_dm_says_it_was_left_rather_than_delivered() {
        let (context, events, _dir) = context_with_overlay_teammates().await;
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("brand".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["copy"],
                    "message": "ship it"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        let said = format!("{result:?}");
        assert!(said.contains("Left for @copy"), "{said}");
        assert!(said.contains("next turn"), "{said}");
        assert!(
            !said.to_lowercase().contains("delivered now\" "),
            "the sentence must not read as delivery: {said}"
        );
        drop(events);
    }

    /// Self-addressing is refused in three places in the crate and is refused
    /// here too, at the one point that holds the speaker's own id. A row whose
    /// audience is only its author is a covert channel with a journal entry.
    #[tokio::test]
    async fn a_dm_to_yourself_is_refused() {
        let (context, events, _dir) = context_with_overlay_teammates().await;
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken, async {
            crate::runtime::delegation::with_turn_conversation(
                Some("brand".to_string()),
                DmTool(context).execute(serde_json::json!({
                    "to": ["designer"],
                    "message": "note to self"
                })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(result.is_error, "{result:?}");
        assert!(events.0.lock().expect("lock").is_empty());
    }

    /// A turn with no conversation has no channel to speak into. Posting into a
    /// guessed one would put a line in front of people who were not in the
    /// exchange, so this refuses rather than defaulting.
    #[tokio::test]
    async fn speaking_outside_a_channel_is_refused() {
        let (context, events, _dir) = context();
        let result = PostTool(context)
            .execute(serde_json::json!({ "message": "anyone there?" }))
            .await
            .expect("the tool runs");
        assert!(result.is_error, "{result:?}");
        assert!(events.0.lock().expect("lock").is_empty());
    }

    /// An empty message is not silence, it is a mistake — and saying so is
    /// better than journaling a blank row.
    #[tokio::test]
    async fn an_empty_message_is_refused() {
        let (context, _events, _dir) = context();
        let spoken = crate::runtime::delegation::new_turn_speech();
        let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
            crate::runtime::delegation::with_turn_conversation(
                Some("brand".to_string()),
                PostTool(context).execute(serde_json::json!({ "message": "   " })),
            )
            .await
        })
        .await
        .expect("the tool runs");
        assert!(result.is_error, "{result:?}");
        assert!(!spoken.spoke());
    }

    // -------------------------------------------------------------------
    // `desk_read` — tinysweeper: the only speech tool with no coverage of
    // its scan, its channel/audience filtering, or its truncation footer.
    // -------------------------------------------------------------------

    /// [`context_with_desks`], but wired to a [`HistoryLog`] instead of a
    /// [`RecordingLog`] — `desk_read` is the one tool that actually reads
    /// history back, and `RecordingLog::read_from` hardcodes an empty page.
    async fn context_with_desks_history() -> (SpeechContext, Arc<HistoryLog>, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("speech-tools-")
            .tempdir()
            .expect("tempdir");
        let store: Arc<dyn crate::ports::store::CompanyStore> =
            Arc::new(crate::store::FsCompanyStore::new(dir.path()));
        let events = Arc::new(HistoryLog::default());
        let company = CompanyId::new("acme");
        let context = SpeechContext::new(
            company.clone(),
            "designer".to_string(),
            events.clone() as Arc<dyn EventLog>,
            store.clone(),
        );
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "designer"
role = "Designer"
description = "Draws things."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds things."

[[group_chat]]
id = "brand"
name = "Brand"
members = ["designer"]

[[group_chat]]
id = "platform"
name = "Platform"
members = ["engineer"]
"#,
        )
        .expect("valid manifest");
        let record = crate::ports::types::CompanyRecord {
            id: company,
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_tool_grants: None,
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        };
        store.save(&record).await.expect("the record saves");
        (context, events, dir)
    }

    fn brand_reply(agent_id: &str, text: &str, audience: Vec<String>) -> CompanyEvent {
        CompanyEvent::AgentReply {
            chat_id: "brand".to_string(),
            agent_id: agent_id.to_string(),
            text: text.to_string(),
            steps: Vec::new(),
            task_id: None,
            parent: None,
            mentions: Vec::new(),
            mention_depth: 0,
            audience,
        }
    }

    /// The scan returns the channel's own rows, oldest first, and leaves
    /// another channel's rows out.
    #[tokio::test]
    async fn desk_read_returns_this_channels_recent_messages_in_order() {
        let (context, events, _dir) = context_with_desks_history().await;
        events
            .append(
                &context.company,
                CompanyEvent::AgentReply {
                    chat_id: "platform".to_string(),
                    agent_id: "engineer".to_string(),
                    text: "not brand's business".to_string(),
                    steps: Vec::new(),
                    task_id: None,
                    parent: None,
                    mentions: Vec::new(),
                    mention_depth: 0,
                    audience: Vec::new(),
                },
            )
            .await
            .expect("seeded");
        for text in ["first", "second", "third"] {
            events
                .append(&context.company, brand_reply("designer", text, Vec::new()))
                .await
                .expect("seeded");
        }
        let result = crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            ReadTool(context).execute(serde_json::json!({})),
        )
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let text = tool_result_text(&result);
        let first = text.find("first").expect("first present");
        let second = text.find("second").expect("second present");
        let third = text.find("third").expect("third present");
        assert!(
            first < second && second < third,
            "rows must read oldest-first: {text}"
        );
        assert!(
            !text.contains("not brand's business"),
            "another channel's row must not leak into this one's read: {text}"
        );
    }

    /// A private aside this agent was not addressed on is narrowed out —
    /// the same audience rule the session delta applies.
    #[tokio::test]
    async fn desk_read_narrows_an_aside_by_audience() {
        let (context, events, _dir) = context_with_desks_history().await;
        events
            .append(
                &context.company,
                brand_reply("designer", "public line", Vec::new()),
            )
            .await
            .expect("seeded");
        events
            .append(
                &context.company,
                brand_reply(
                    "engineer",
                    "a private aside between us, not designer",
                    vec!["someone_else".to_string()],
                ),
            )
            .await
            .expect("seeded");
        let result = crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            ReadTool(context).execute(serde_json::json!({})),
        )
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let text = tool_result_text(&result);
        assert!(text.contains("public line"), "{text}");
        assert!(
            !text.contains("a private aside between us"),
            "an aside this agent is not in the audience of must not be readable by asking \
             for more of the channel: {text}"
        );
    }

    /// tinysweeper: `truncated` must fire when the scan exhausts its budget
    /// before `limit` matching messages are found, not only when it finds
    /// `limit` of them — a partial list that reads as complete is worse than
    /// an honest "did not look far enough".
    #[tokio::test]
    async fn desk_read_reports_truncation_when_the_scan_budget_is_exhausted() {
        let (context, events, _dir) = context_with_desks_history().await;
        // Filler on a channel `desk_read` never matches, so every one of these
        // is scanned and none is returned — exhausting `SEARCH_BUDGET` (2048)
        // well before the default `limit` of matching `brand` rows is found.
        for _ in 0..2100 {
            events
                .append(
                    &context.company,
                    CompanyEvent::AgentReply {
                        chat_id: "platform".to_string(),
                        agent_id: "engineer".to_string(),
                        text: "filler".to_string(),
                        steps: Vec::new(),
                        task_id: None,
                        parent: None,
                        mentions: Vec::new(),
                        mention_depth: 0,
                        audience: Vec::new(),
                    },
                )
                .await
                .expect("seeded");
        }
        events
            .append(
                &context.company,
                brand_reply("designer", "buried under the filler", Vec::new()),
            )
            .await
            .expect("seeded");
        let result = crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            ReadTool(context).execute(serde_json::json!({})),
        )
        .await
        .expect("the tool runs");
        assert!(!result.is_error, "{result:?}");
        let text = tool_result_text(&result);
        assert!(
            text.contains("Older messages are not in this reply"),
            "budget exhaustion must be reported as truncation, not read as a complete \
             (and in this case entirely empty) channel: {text}"
        );
    }
}
