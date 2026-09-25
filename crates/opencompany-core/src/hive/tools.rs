//! The in-flight turn registry, the speech fold and the tool adapter behind
//! the hive MCP server (plan `hive-desks`, Phase 3).
//!
//! # Attribution
//!
//! An `openhuman_embed::Agent` reaches OpenCompany's tools over MCP, so the
//! server receives a bearer and a tool call and nothing else — no episode, no
//! round, no conversation. What ties the call back to the turn it belongs to
//! is this registry: **one agent runs at most one turn at a time** (the pool's
//! `turn_lock`), so `runtime_agent_id → exactly one [`InFlight`]` is a total
//! function for the life of the turn, and the bearer names the agent. The
//! driver registers an [`InFlight`] before it calls `agent.turn(..)`, the
//! handler looks it up on every call, and the driver takes the outbox back
//! when the turn settles.
//!
//! # One action per turn
//!
//! A seat says exactly one thing per turn. The first speech call is folded
//! through [`tinyhivemind::speech::interpret`] and recorded on the outbox; a
//! second is refused with a tool error the seat can read inside its own turn
//! (the wording is pinned by Track B's mock brain, see [`speak`](InFlight::speak)).
//! `read` is not an action and never counts.
//!
//! # `dm` recipients
//!
//! A `dm` is checked against the desk membership the driver captured at turn
//! start ([`HiveTurn::members`]). A refused `dm` is a tool ERROR whose text
//! says `refused` — Track B's mock brain matches that. Phase 4's `DeskHive`
//! carries the authoritative `resolve_dm`; until it installs a
//! [`DmResolver`](crate::hive::mcp_server::DmResolver) on the host, the
//! membership snapshot is the rule.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{Value, json};
use tinyhivemind::speech::{
    self, CallArguments, ParameterKind, ToolCall, ToolSpec, Utterance, UtteranceRejection,
};
use tinyhivemind_embed::ConversationRef;
use tinytools::{Tool, ToolCallOptions, ToolResult, ToolRunContext, WorkspaceDescriptor};

use crate::ports::types::CompanyId;

/// The bare speech tool names, in the order [`speech::tool_specs`] presents
/// them. Every other name the server serves is an OpenCompany tool.
#[must_use]
pub fn speech_tool_names() -> Vec<&'static str> {
    speech::tool_specs().iter().map(|spec| spec.name).collect()
}

/// The speech tools a room actually offers a seat.
///
/// Narrower than [`speech_tool_names`], which is the whole vocabulary. `post`
/// and `dm` are defined and deliberately withheld: `post` is text with no
/// consequence, and five live runs spent it on status, on restating findings
/// the seat completed with anyway, and on describing calls it had not made;
/// `dm` beside `ask` is two ways to say nearly the same thing.
///
/// Use this wherever this crate **advertises** the vocabulary -- the MCP
/// brief's `Tools:` line, the definition's tool scope -- so a seat is never
/// told about a tool the room will refuse. Classification stays on the wider
/// list: a call to a withheld name is still a speech call, and still the
/// room's to refuse rather than the company's to mistake for one of its own.
#[must_use]
pub fn served_speech_tool_names() -> Vec<&'static str> {
    tinyhivemind_tools::served_specs()
        .map(|spec| spec.name)
        .collect()
}

/// Whether `name` is one of the room's speech tools.
#[must_use]
pub fn is_speech_tool(name: &str) -> bool {
    speech_tool_names().contains(&name)
}

/// The hive coordinates of a desk turn: which episode and round the seat is
/// answering in, and who else is on the desk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HiveTurn {
    /// The desk whose hive runs this episode.
    pub desk_id: String,
    /// The episode the round belongs to.
    pub episode_id: String,
    /// The round revision (raw, 0-based — displayed 1-based by the console).
    pub revision: u64,
    /// The turn id the `turn_started` / `turn_settled` frames are keyed by.
    pub turn_id: String,
    /// The desk's current members, the `dm` recipient rule until Phase 4's
    /// `resolve_dm` is installed. The speaker may be in the list.
    pub members: Vec<String>,
}

/// One turn in flight for one agent: what the MCP handler needs to attribute a
/// call and where the seat's one utterance lands.
#[derive(Clone, Debug)]
pub struct InFlight {
    /// The company the agent belongs to.
    pub company: CompanyId,
    /// The runtime agent id (`{company}--{agent}`) the bearer resolves to.
    pub runtime_agent_id: String,
    /// The manifest agent id — the speaker id the room knows.
    pub agent_id: String,
    /// The conversation the turn answers in.
    pub surface: ConversationRef,
    /// The episode/round, for a desk turn driven by a hive; `None` for a
    /// direct, general or workflow turn, which has no round and no `dm`.
    pub hive: Option<HiveTurn>,
    /// What the seat said this turn — at most one utterance.
    pub outbox: Vec<Utterance>,
    /// Where an OpenCompany tool call is handed so it runs on the turn's own
    /// task (see [`ToolJob`]). `None` — a test, or a caller that registered
    /// the turn without running one — has the server run the call itself.
    pub executor: Option<ToolJobSender>,
}

/// One OpenCompany tool call, handed from the MCP server's task to the turn's.
///
/// The belt's tools file into task-local queues — the approval scope, the
/// publish and delegation claims, the explicit-request boundary — that are
/// set on the task the turn runs on and invisible from the server's. Rather
/// than carry each of those across (and miss the next one), the server hands
/// the call back to the turn's task, which decides it under the agent's
/// policy and runs it there; the reply comes back on `reply`.
pub struct ToolJob {
    /// The tool the model called.
    pub tool: String,
    /// The arguments object it passed.
    pub arguments: Value,
    /// Where the MCP tool result (the `tools/call` result member) goes. A
    /// dropped sender means the turn ended before the call ran.
    pub reply: tokio::sync::oneshot::Sender<Value>,
}

impl fmt::Debug for ToolJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolJob")
            .field("tool", &self.tool)
            .finish_non_exhaustive()
    }
}

/// The turn-side end of the hand-off; the turn's task drains the receiver.
pub type ToolJobSender = tokio::sync::mpsc::Sender<ToolJob>;

/// What one speech call became.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Speech {
    /// The utterance was recorded on the outbox; the text is the seat's receipt.
    Recorded(String),
    /// A `read`, which the handler serves from the session log.
    Read {
        /// How many recent messages the seat asked for, already clamped.
        limit: usize,
    },
    /// The call was refused. Rendered as a tool ERROR; the text is written to
    /// be read by the seat and always contains `refused`.
    Refused(String),
}

impl InFlight {
    /// A turn with an empty outbox and no hive coordinates.
    #[must_use]
    pub fn new(
        company: CompanyId,
        runtime_agent_id: impl Into<String>,
        agent_id: impl Into<String>,
        surface: ConversationRef,
    ) -> Self {
        Self {
            company,
            runtime_agent_id: runtime_agent_id.into(),
            agent_id: agent_id.into(),
            surface,
            hive: None,
            outbox: Vec::new(),
            executor: None,
        }
    }

    /// Attaches the hive coordinates of a desk round.
    #[must_use]
    pub fn with_hive(mut self, hive: HiveTurn) -> Self {
        self.hive = Some(hive);
        self
    }

    /// Hands OpenCompany tool calls to the turn's task through `executor`.
    #[must_use]
    pub fn with_executor(mut self, executor: ToolJobSender) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Folds one speech call: `speech::interpret` over the wire arguments,
    /// then the two host rules — one action per turn, and a `dm` may only
    /// name desk members other than the speaker.
    pub fn speak(&mut self, name: &str, arguments: &Value) -> Speech {
        let message = arguments.get("message").and_then(Value::as_str);
        let to: Vec<String> = arguments
            .get("to")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let limit = arguments.get("limit").and_then(Value::as_u64);
        let call = speech::interpret(
            name,
            &CallArguments {
                message,
                to: &to,
                limit,
            },
        );
        let utterance = match call {
            Ok(ToolCall::Read { limit }) => return Speech::Read { limit },
            Ok(ToolCall::Speak(utterance)) => utterance,
            Err(rejection) => return Speech::Refused(format!("refused: {rejection}")),
        };
        if let Some(first) = self.outbox.first() {
            return Speech::Refused(format!(
                "refused: one action per turn; your first action is recorded ({}). End your \
                 turn now — say nothing else.",
                kind_of(first)
            ));
        }
        if let Err(reason) = self.check_dm(&utterance) {
            return Speech::Refused(format!("refused: {reason}"));
        }
        let receipt = format!(
            "recorded: {} ({} chars). Your turn is complete; say nothing else.",
            kind_of(&utterance),
            utterance.message().chars().count()
        );
        self.outbox.push(utterance);
        Speech::Recorded(receipt)
    }

    /// The `dm` rule: only inside a desk episode, only to members of that desk,
    /// never only to oneself. Mirrors [`speech::check_recipients`] over the
    /// captured membership instead of a live roster.
    fn check_dm(&self, utterance: &Utterance) -> Result<(), String> {
        let Utterance::Dm { to, .. } = utterance else {
            return Ok(());
        };
        let Some(hive) = &self.hive else {
            return Err(
                "`dm` is only available inside a desk episode; this turn is not in one".to_string(),
            );
        };
        for id in to {
            if !hive.members.iter().any(|member| member == id) {
                return Err(UtteranceRejection::UnknownRecipient { id: id.clone() }.to_string());
            }
        }
        if to.iter().all(|id| id == &self.agent_id) {
            return Err(UtteranceRejection::SelfRecipient.to_string());
        }
        Ok(())
    }
}

/// The wire kind of an utterance, as `Utterance`'s serde tag spells it.
fn kind_of(utterance: &Utterance) -> &'static str {
    match utterance {
        Utterance::Post { .. } => "post",
        Utterance::Broadcast { .. } => "broadcast",
        Utterance::Dm { .. } => "dm",
        // New with the conductor: a private question to one seat, which
        // opens a conversation only those two read.
        Utterance::Ask { .. } => "ask",
        Utterance::CompleteEpisode { .. } => "complete_episode",
    }
}

/// Why a turn could not be registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InFlightError {
    /// The agent already has a turn in flight — the rule is one at a time.
    #[error("agent '{runtime_agent_id}' already has a turn in flight")]
    AlreadyInFlight {
        /// The agent that was busy.
        runtime_agent_id: String,
    },
}

/// The turns currently in flight, keyed by runtime agent id.
///
/// A `std::sync::RwLock`: every critical section is a lookup or an insert with
/// no `await` in it, and the handler runs on axum's task, not the turn's.
#[derive(Default)]
pub struct InFlightRegistry {
    turns: RwLock<HashMap<String, Arc<Mutex<InFlight>>>>,
}

impl fmt::Debug for InFlightRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys: Vec<String> = self
            .turns
            .read()
            .map_or_else(|_| Vec::new(), |turns| turns.keys().cloned().collect());
        f.debug_struct("InFlightRegistry")
            .field("in_flight", &keys)
            .finish()
    }
}

impl InFlightRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `turn` and returns the ticket that owns it. Refused when the
    /// agent already has a turn in flight.
    pub fn begin(self: &Arc<Self>, turn: InFlight) -> Result<InFlightTicket, InFlightError> {
        let runtime_agent_id = turn.runtime_agent_id.clone();
        let mut turns = self.turns.write().expect("in-flight registry poisoned");
        if turns.contains_key(&runtime_agent_id) {
            return Err(InFlightError::AlreadyInFlight { runtime_agent_id });
        }
        let slot = Arc::new(Mutex::new(turn));
        turns.insert(runtime_agent_id.clone(), slot.clone());
        Ok(InFlightTicket {
            registry: self.clone(),
            runtime_agent_id,
            slot,
        })
    }

    /// Runs `f` against the agent's in-flight turn, or `None` when it has none.
    pub fn with<T>(&self, runtime_agent_id: &str, f: impl FnOnce(&mut InFlight) -> T) -> Option<T> {
        let slot = self
            .turns
            .read()
            .expect("in-flight registry poisoned")
            .get(runtime_agent_id)
            .cloned()?;
        let mut turn = slot.lock().expect("in-flight turn poisoned");
        Some(f(&mut turn))
    }

    /// A copy of the agent's in-flight turn, if any.
    #[must_use]
    pub fn snapshot(&self, runtime_agent_id: &str) -> Option<InFlight> {
        self.with(runtime_agent_id, |turn| turn.clone())
    }

    /// Whether the agent has a turn in flight.
    #[must_use]
    pub fn is_in_flight(&self, runtime_agent_id: &str) -> bool {
        self.turns
            .read()
            .expect("in-flight registry poisoned")
            .contains_key(runtime_agent_id)
    }

    /// The runtime agent ids with a turn in flight, sorted.
    #[must_use]
    pub fn in_flight(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .turns
            .read()
            .expect("in-flight registry poisoned")
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    fn remove(&self, runtime_agent_id: &str) -> Option<Arc<Mutex<InFlight>>> {
        self.turns
            .write()
            .expect("in-flight registry poisoned")
            .remove(runtime_agent_id)
    }
}

/// Ownership of one registered turn. Dropping it deregisters the turn, so a
/// turn that panics or is cancelled never leaves its agent "busy" forever;
/// [`finish`](Self::finish) deregisters it and hands the outbox back.
pub struct InFlightTicket {
    registry: Arc<InFlightRegistry>,
    runtime_agent_id: String,
    slot: Arc<Mutex<InFlight>>,
}

impl fmt::Debug for InFlightTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InFlightTicket")
            .field("runtime_agent_id", &self.runtime_agent_id)
            .finish_non_exhaustive()
    }
}

impl InFlightTicket {
    /// The runtime agent id this ticket holds.
    #[must_use]
    pub fn runtime_agent_id(&self) -> &str {
        &self.runtime_agent_id
    }

    /// The turn as it stands now (the outbox so far).
    #[must_use]
    pub fn snapshot(&self) -> InFlight {
        self.slot.lock().expect("in-flight turn poisoned").clone()
    }

    /// Deregisters the turn and returns its final state, outbox included.
    #[must_use]
    pub fn finish(self) -> InFlight {
        self.registry.remove(&self.runtime_agent_id);
        let turn = self.slot.lock().expect("in-flight turn poisoned").clone();
        // `Drop` would remove again; the entry is already gone, so it is a
        // no-op there. Nothing else to release.
        turn
    }
}

impl Drop for InFlightTicket {
    fn drop(&mut self) {
        // Only this ticket's own slot: a `finish` already removed it, and a
        // later `begin` for the same agent must not be evicted by a stale drop.
        let mut turns = self
            .registry
            .turns
            .write()
            .expect("in-flight registry poisoned");
        if turns
            .get(&self.runtime_agent_id)
            .is_some_and(|slot| Arc::ptr_eq(slot, &self.slot))
        {
            turns.remove(&self.runtime_agent_id);
        }
    }
}

/// The run context a served OpenCompany tool executes under: the in-flight
/// turn (when the agent has one) and its workspace.
///
/// `host_extension` returns `self`, so a tool written against this host can
/// downcast to it and read [`turn`](Self::turn); every portable question goes
/// through the [`ToolRunContext`] methods.
#[derive(Clone, Debug)]
pub struct InFlightContext {
    /// The turn the call was attributed to, or `None` for a call made outside
    /// any registered turn (a tool exercised by a test, or a surface the
    /// driver has not registered yet).
    pub turn: Option<InFlight>,
    /// The agent workspace the file-scoped tools sandbox to.
    workspace: Option<WorkspaceDescriptor>,
}

impl InFlightContext {
    /// A context over `turn` and the agent workspace at `workspace`.
    #[must_use]
    pub fn new(turn: Option<InFlight>, workspace: Option<PathBuf>) -> Self {
        Self {
            turn,
            workspace: workspace.map(|root| WorkspaceDescriptor {
                root,
                trusted_roots: Vec::new(),
                policy_id: "opencompany-agent-workspace".to_string(),
                sandbox: Default::default(),
            }),
        }
    }
}

impl ToolRunContext for InFlightContext {
    fn workspace(&self) -> Option<&WorkspaceDescriptor> {
        self.workspace.as_ref()
    }

    fn thread_id(&self) -> Option<&str> {
        self.turn.as_ref().map(|turn| turn.surface.id.as_str())
    }

    fn host_extension(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        Some(self)
    }
}

/// One OpenCompany tool served over MCP: the name, description and schema
/// the catalogue lists, and `execute` under the turn's context.
#[derive(Clone)]
pub struct McpToolAdapter {
    /// The belt tool, shared with the pool that built it.
    pub tool: Arc<dyn Tool>,
}

impl fmt::Debug for McpToolAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpToolAdapter")
            .field("name", &self.tool.name())
            .finish()
    }
}

impl McpToolAdapter {
    /// Wraps one belt tool.
    #[must_use]
    pub fn new(tool: Arc<dyn Tool>) -> Self {
        Self { tool }
    }

    /// The tool name, as the catalogue lists it.
    #[must_use]
    pub fn name(&self) -> &str {
        self.tool.name()
    }

    /// The MCP tool descriptor: `name`, `description`, `inputSchema`.
    #[must_use]
    pub fn descriptor(&self) -> Value {
        json!({
            "name": self.tool.name(),
            "description": self.tool.description(),
            "inputSchema": self.tool.parameters_schema(),
        })
    }

    /// Runs the tool. A tool that could not run at all (`Err`) is reported
    /// to the seat as a tool error rather than dropped on the floor.
    pub async fn execute(&self, arguments: Value, context: &InFlightContext) -> ToolResult {
        match self
            .tool
            .execute_with_context(arguments, ToolCallOptions::default(), Some(context))
            .await
        {
            Ok(result) => result,
            Err(error) => ToolResult::error(format!("'{}' failed: {error}", self.tool.name())),
        }
    }
}

/// Moves a built belt into shared handles, so the pool can keep one and hand
/// the MCP host another.
#[must_use]
pub fn share_belt(belt: Vec<Box<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
    belt.into_iter().map(Arc::from).collect()
}

/// How a model reaches `tool` on a company agent — the shape the scripted
/// models in this crate's turn tests emit, and the console's mock brain.
///
/// **Almost everything is a bare name now.** This crate's own tools ride the
/// agent's belt directly (`AgentSpec::tools`), so a model calls them the way
/// it calls a shell: by name, against their own schema. Only the speech tools
/// are still served over the `opencompany` server, because only a seat in an
/// episode answers with one, and they go on the wire wrapped in
/// `mcp_call_tool` with `args` as the arguments object.
#[must_use]
pub fn via_opencompany_mcp(tool: &str, args: Value) -> (String, Value) {
    if !speech_tool_names().contains(&tool) {
        return (tool.to_string(), args);
    }
    (
        "mcp_call_tool".to_string(),
        json!({
            "server": crate::hive::mcp_server::SERVER_SLUG,
            "tool": tool,
            "arguments": args,
        }),
    )
}

/// Renders one speech spec to an MCP tool descriptor.
#[must_use]
pub fn speech_descriptor(spec: &ToolSpec) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<&str> = Vec::new();
    for parameter in spec.parameters {
        let mut schema = match parameter.kind {
            ParameterKind::Text => json!({ "type": "string" }),
            ParameterKind::TextList => json!({ "type": "array", "items": { "type": "string" } }),
            ParameterKind::Count { default, min, max } => json!({
                "type": "integer", "minimum": min, "maximum": max, "default": default
            }),
        };
        if let Some(description) = parameter.description {
            schema["description"] = Value::String(description.to_string());
        }
        properties.insert(parameter.name.to_string(), schema);
        if parameter.required {
            required.push(parameter.name);
        }
    }
    json!({
        "name": spec.name,
        "description": spec.description,
        "inputSchema": {
            "type": "object",
            "properties": Value::Object(properties),
            "required": required,
            "additionalProperties": false,
        },
    })
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
