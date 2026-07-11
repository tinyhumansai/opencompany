//! The `/orchestration/v1` wire contract as typed serde.
//!
//! Every request body carries `"protocol": 1` (see [`Envelope`]); responses are
//! `{ success, data }` or `{ success, error, errorCode?, details? }` (see
//! [`ApiResponse`]). Field names on the wire are camelCase; the Rust structs use
//! `snake_case` with `#[serde(rename_all = "camelCase")]`.
//!
//! The shapes here are pinned by the round-trip tests at the bottom of the file:
//! if the live backend disagrees on a shape, only this module changes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::error::OpenCompanyError;

// ---------------------------------------------------------------------------
// Protocol + envelope
// ---------------------------------------------------------------------------

/// The wire protocol version every request declares.
pub const PROTOCOL: u8 = 1;

/// The minimum protocol the server accepts today.
pub const PROTOCOL_MIN: u8 = 1;

/// The maximum protocol the server accepts today.
pub const PROTOCOL_MAX: u8 = 1;

/// A request body wrapped with its `"protocol"` version.
///
/// The inner `body` is flattened, so `Envelope::v1(EventsRequest { … })`
/// serializes to `{ "protocol": 1, "counterpartAgentId": …, … }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// The wire protocol version. Always [`PROTOCOL`] for requests we send.
    pub protocol: u8,
    /// The wrapped request body.
    #[serde(flatten)]
    pub body: T,
}

impl<T> Envelope<T> {
    /// Wraps `body` at protocol version 1.
    pub fn v1(body: T) -> Self {
        Self {
            protocol: PROTOCOL,
            body,
        }
    }
}

// ---------------------------------------------------------------------------
// Response envelope + error codes
// ---------------------------------------------------------------------------

/// A decoded `{ success, data | error }` response envelope.
///
/// The `success` boolean discriminates: a `true` envelope carries `data`, a
/// `false` envelope carries `error`/`errorCode`/`details`. Use
/// [`ApiResponse::into_result`] to collapse it into a crate [`Result`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse<T> {
    /// Whether the call succeeded.
    pub success: bool,
    /// The success payload, present when `success` is `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    /// The human-readable error message, present when `success` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The `ORCH_*` error code, present on most error envelopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Structured error detail (e.g. `{min,max}` for a protocol mismatch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl<T> ApiResponse<T> {
    /// Wraps a success payload.
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            error_code: None,
            details: None,
        }
    }

    /// Collapses the envelope into a crate [`Result`].
    ///
    /// A `success: false` envelope maps to
    /// [`OpenCompanyError::Orchestration`], preserving the verbatim `ORCH_*`
    /// code and folding any `details` into the message so `{min,max}` and
    /// friends survive. A `success: true` envelope with no `data` is treated as
    /// a validation error from the server.
    pub fn into_result(self) -> Result<T> {
        if self.success {
            return self.data.ok_or_else(|| {
                OrchErrorCode::ValidationError.to_error("success envelope had no data")
            });
        }
        let code = self
            .error_code
            .as_deref()
            .map(OrchErrorCode::from_wire)
            .unwrap_or(OrchErrorCode::Unknown(String::new()));
        let mut message = self
            .error
            .unwrap_or_else(|| "orchestration error".to_string());
        if let Some(details) = &self.details {
            message = format!("{message} ({details})");
        }
        Err(code.to_error(message))
    }
}

/// The nine `ORCH_*` error codes the v1 orchestrator can return, plus a
/// forward-compatible [`OrchErrorCode::Unknown`] carrying any future code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrchErrorCode {
    /// `ORCH_PROTOCOL_MISMATCH` — the request protocol is out of range.
    ProtocolMismatch,
    /// `ORCH_MODEL_NOT_ALLOWED` — the request tried to select a model.
    ModelNotAllowed,
    /// `ORCH_VALIDATION_ERROR` — the request body failed validation.
    ValidationError,
    /// `ORCH_INSUFFICIENT_BALANCE` — the account cannot fund the cycle.
    InsufficientBalance,
    /// `ORCH_RATE_LIMITED` — the account is being throttled.
    RateLimited,
    /// `ORCH_UPSTREAM_MODEL_ERROR` — a backing model provider failed.
    UpstreamModelError,
    /// `ORCH_INVALID_STATE` — the session is in a state that rejects the call.
    InvalidState,
    /// `ORCH_DEVICE_OFFLINE` — the device socket is not connected.
    DeviceOffline,
    /// `ORCH_EXECUTE_TIMEOUT` — a device tool exceeded its budget.
    ExecuteTimeout,
    /// Any code the client does not recognize, stored verbatim.
    Unknown(String),
}

impl OrchErrorCode {
    /// The verbatim `ORCH_*` wire string for this code.
    pub fn as_str(&self) -> &str {
        match self {
            Self::ProtocolMismatch => "ORCH_PROTOCOL_MISMATCH",
            Self::ModelNotAllowed => "ORCH_MODEL_NOT_ALLOWED",
            Self::ValidationError => "ORCH_VALIDATION_ERROR",
            Self::InsufficientBalance => "ORCH_INSUFFICIENT_BALANCE",
            Self::RateLimited => "ORCH_RATE_LIMITED",
            Self::UpstreamModelError => "ORCH_UPSTREAM_MODEL_ERROR",
            Self::InvalidState => "ORCH_INVALID_STATE",
            Self::DeviceOffline => "ORCH_DEVICE_OFFLINE",
            Self::ExecuteTimeout => "ORCH_EXECUTE_TIMEOUT",
            Self::Unknown(code) => code,
        }
    }

    /// Parses a wire `ORCH_*` string, mapping unknown codes to
    /// [`OrchErrorCode::Unknown`].
    pub fn from_wire(code: &str) -> Self {
        match code {
            "ORCH_PROTOCOL_MISMATCH" => Self::ProtocolMismatch,
            "ORCH_MODEL_NOT_ALLOWED" => Self::ModelNotAllowed,
            "ORCH_VALIDATION_ERROR" => Self::ValidationError,
            "ORCH_INSUFFICIENT_BALANCE" => Self::InsufficientBalance,
            "ORCH_RATE_LIMITED" => Self::RateLimited,
            "ORCH_UPSTREAM_MODEL_ERROR" => Self::UpstreamModelError,
            "ORCH_INVALID_STATE" => Self::InvalidState,
            "ORCH_DEVICE_OFFLINE" => Self::DeviceOffline,
            "ORCH_EXECUTE_TIMEOUT" => Self::ExecuteTimeout,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Builds an [`OpenCompanyError::Orchestration`] carrying this code.
    pub fn to_error(&self, message: impl Into<String>) -> OpenCompanyError {
        OpenCompanyError::orchestration(self.as_str(), message)
    }
}

// ---------------------------------------------------------------------------
// POST /events
// ---------------------------------------------------------------------------

/// The role of an event's author, as the orchestrator sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The human operator or an inbound counterparty.
    User,
    /// The company/agent itself.
    Assistant,
    /// The runtime (timers, boot replay, system notices).
    System,
}

/// A single normalized event delivered to `POST /events`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireEvent {
    /// The event's monotonic sequence within the session (the `EventLog` seq).
    pub seq: u64,
    /// Who authored the event.
    pub role: Role,
    /// A stable id for the sender (1..256 chars).
    pub sender: String,
    /// The plaintext event body (≤200000 chars).
    pub body: String,
    /// Epoch-millis timestamp.
    pub ts: i64,
    /// The dotted event kind (1..64 chars).
    pub kind: String,
}

/// The `POST /events` request body (wrap in [`Envelope::v1`] to send).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsRequest {
    /// The counterpart agent id, e.g. `opencompany:<slug>` (1..256 chars).
    pub counterpart_agent_id: String,
    /// The session id — one session per company (1..256 chars).
    pub session_id: String,
    /// The event that triggers the wake.
    pub event: WireEvent,
}

/// The `202` acknowledgement returned by `POST /events`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsAccepted {
    /// Whether the event was accepted for processing.
    pub accepted: bool,
    /// The deterministic cycle id the wake fires under.
    pub cycle_id: String,
}

/// Builds the deterministic cycle id `cyc:<counterpart>:<session>:<seq>`.
///
/// Ingest is idempotent on `(user, counterpart, session, seq)`; this id is the
/// stable key derived from those coordinates.
pub fn cycle_id(counterpart: &str, session: &str, seq: u64) -> String {
    format!("cyc:{counterpart}:{session}:{seq}")
}

// ---------------------------------------------------------------------------
// POST /world-diff
// ---------------------------------------------------------------------------

/// The maximum number of world-diff entries a single request may carry.
pub const WORLD_DIFF_MAX_ENTRIES: usize = 500;

/// The maximum length of a single world-diff note, in characters.
pub const WORLD_DIFF_MAX_NOTE: usize = 8000;

/// One world-state note uploaded via `POST /world-diff`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldDiffEntry {
    /// The sequence the note is anchored to.
    pub seq: u64,
    /// The world-state note (≤8000 chars).
    pub note: String,
    /// Epoch-millis timestamp.
    pub ts: i64,
}

/// The `POST /world-diff` request body (wrap in [`Envelope::v1`] to send).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldDiffRequest {
    /// The session the notes belong to.
    pub session_id: String,
    /// The notes to upload (1..=500 entries).
    pub entries: Vec<WorldDiffEntry>,
}

impl WorldDiffRequest {
    /// Validates the entry-count and note-length bounds the server enforces.
    ///
    /// Returns [`OrchErrorCode::ValidationError`] on a violation so the caller
    /// fails fast without a round-trip.
    pub fn validate(&self) -> Result<()> {
        if self.entries.is_empty() {
            return Err(
                OrchErrorCode::ValidationError.to_error("world-diff requires at least one entry")
            );
        }
        if self.entries.len() > WORLD_DIFF_MAX_ENTRIES {
            return Err(OrchErrorCode::ValidationError.to_error(format!(
                "world-diff exceeds {WORLD_DIFF_MAX_ENTRIES} entries"
            )));
        }
        for entry in &self.entries {
            if entry.note.chars().count() > WORLD_DIFF_MAX_NOTE {
                return Err(OrchErrorCode::ValidationError.to_error(format!(
                    "world-diff note exceeds {WORLD_DIFF_MAX_NOTE} chars"
                )));
            }
        }
        Ok(())
    }
}

/// The `202` acknowledgement returned by `POST /world-diff`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldDiffAccepted {
    /// Whether the batch was accepted.
    pub accepted: bool,
    /// How many entries were duplicates of already-seen seqs.
    pub duplicates: u32,
    /// Whether a subconscious tick was scheduled as a result.
    pub tick_scheduled: bool,
}

// ---------------------------------------------------------------------------
// Read surface
// ---------------------------------------------------------------------------

/// A row from `GET /sessions`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    /// The session id.
    pub session_id: String,
    /// The session's lifecycle status.
    pub status: String,
    /// The last sequence the server has ingested for this session.
    pub last_seq: u64,
}

/// A row from `GET /sessions/:id/messages`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageView {
    /// The message sequence.
    pub seq: u64,
    /// Who authored the message.
    pub role: Role,
    /// The sender id.
    pub sender: String,
    /// The message body.
    pub body: String,
    /// Epoch-millis timestamp.
    pub ts: i64,
    /// The dotted message kind.
    pub kind: String,
}

/// The body of `GET /sessions/:id/state`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    /// The session id.
    pub session_id: String,
    /// The session's lifecycle status.
    pub status: String,
    /// The last ingested sequence.
    pub last_seq: u64,
    /// The last cycle id that fired, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_cycle_id: Option<String>,
}

/// A single steering directive from the subconscious tick.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SteeringDirective {
    /// The steering text biasing future wakes.
    pub directive: String,
    /// Epoch-millis timestamp the directive was minted.
    pub ts: i64,
}

/// The body of `GET /steering`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SteeringView {
    /// The currently active directive, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<SteeringDirective>,
    /// Prior directives, newest last.
    pub history: Vec<SteeringDirective>,
}

/// A row from `GET /world-diff?session=<id>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldDiffView {
    /// The sequence the note is anchored to.
    pub seq: u64,
    /// The stored world-state note.
    pub note: String,
    /// Epoch-millis timestamp.
    pub ts: i64,
}

// ---------------------------------------------------------------------------
// Socket.IO frames
// ---------------------------------------------------------------------------

/// The Socket.IO event that registers the device tool manifest.
pub const REGISTER_TOOLS: &str = "orch:register_tools";

/// The Socket.IO event the client emits to ack an effect.
pub const EFFECT_RESULT: &str = "orch:effect:result";

/// The Socket.IO event the server emits to invoke a device tool.
pub const TOOL_CALL: &str = "orch:tool_call";

/// The Socket.IO event the client emits to answer a device tool call.
pub const TOOL_RESULT: &str = "orch:tool_result";

/// The default device-tool timeout budget the server enforces (~30 s).
pub const DEFAULT_TOOL_TIMEOUT_MS: u64 = 30_000;

/// The Socket.IO event name for an effect of the given `kind`.
///
/// e.g. `effect_event_name("send_dm") == "orch:effect:send_dm"`.
pub fn effect_event_name(kind: &str) -> String {
    format!("orch:effect:{kind}")
}

/// Builds the deterministic dedupe key `{cycleId}:{kind}:{index}`.
///
/// Effect delivery is at-least-once; the client dedupes on this id so a replayed
/// frame with the same `(cycle_id, kind, index)` is handled exactly once.
pub fn call_id(cycle_id: &str, kind: &str, index: usize) -> String {
    format!("{cycle_id}:{kind}:{index}")
}

/// One entry in the [`RegisterToolsFrame`] manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolManifestEntry {
    /// The tool name.
    pub name: String,
    /// A human description of the tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON schema for the tool's arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

/// The `orch:register_tools` frame: the device tool catalog.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegisterToolsFrame {
    /// The registered tools.
    pub tools: Vec<ToolManifestEntry>,
}

/// An effect the server pushes over `orch:effect:<kind>`.
///
/// On the wire the frame body is `{ cycleId, callId, …payload }` and the `kind`
/// travels in the Socket.IO event name. This struct carries `kind` alongside for
/// routing; [`EffectFrame::to_wire_body`] / [`EffectFrame::from_wire_body`]
/// translate to and from the spread wire shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectFrame {
    /// The effect kind, taken from the `orch:effect:<kind>` event name.
    pub kind: String,
    /// The cycle this effect belongs to.
    pub cycle_id: String,
    /// The deterministic dedupe id, `{cycleId}:{kind}:{index}`.
    pub call_id: String,
    /// The effect-specific payload (the remaining frame fields).
    pub payload: Value,
}

impl EffectFrame {
    /// The `orch:effect:<kind>` event name this frame is delivered on.
    pub fn event_name(&self) -> String {
        effect_event_name(&self.kind)
    }

    /// Renders the spread wire body `{ cycleId, callId, …payload }`.
    ///
    /// The payload object's keys are merged alongside `cycleId`/`callId`; a
    /// non-object payload is placed under a `payload` key so nothing is lost.
    pub fn to_wire_body(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("cycleId".to_string(), Value::String(self.cycle_id.clone()));
        map.insert("callId".to_string(), Value::String(self.call_id.clone()));
        match &self.payload {
            Value::Object(fields) => {
                for (key, value) in fields {
                    map.insert(key.clone(), value.clone());
                }
            }
            Value::Null => {}
            other => {
                map.insert("payload".to_string(), other.clone());
            }
        }
        Value::Object(map)
    }

    /// Parses a spread wire body for the given effect `kind`.
    ///
    /// `cycleId` and `callId` are lifted out; everything else becomes the
    /// [`EffectFrame::payload`].
    pub fn from_wire_body(kind: impl Into<String>, mut body: Value) -> Result<Self> {
        let obj = body.as_object_mut().ok_or_else(|| {
            OrchErrorCode::ValidationError.to_error("effect frame body was not an object")
        })?;
        let cycle_id = take_string(obj, "cycleId")?;
        let call_id = take_string(obj, "callId")?;
        let payload = if let Some(payload) = obj.remove("payload") {
            payload
        } else {
            Value::Object(std::mem::take(obj))
        };
        Ok(Self {
            kind: kind.into(),
            cycle_id,
            call_id,
            payload,
        })
    }
}

/// The `orch:effect:result` frame the client emits to ack an effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectResult {
    /// The `callId` of the effect being acked.
    pub call_id: String,
    /// Whether the effect was executed.
    pub ok: bool,
    /// An error message when `ok` is `false` (e.g. `"pending approval"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A structured result when the effect produced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// The `orch:tool_call` frame: a mid-cycle device-tool invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallFrame {
    /// The cycle the call belongs to.
    pub cycle_id: String,
    /// The dedupe/correlation id for the call.
    pub call_id: String,
    /// The tool name to invoke.
    pub name: String,
    /// The tool arguments.
    pub args: Value,
    /// The budget the client must answer within (~30 s).
    pub timeout_ms: u64,
}

/// The `orch:tool_result` frame the client emits to answer a tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultFrame {
    /// The `callId` being answered.
    pub call_id: String,
    /// Whether the tool succeeded.
    pub ok: bool,
    /// The tool output, when it succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// An error message, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// model-field guard
// ---------------------------------------------------------------------------

/// Rejects any request body that carries a `model` field.
///
/// The client can never select a model; the server enforces this with
/// `400 ORCH_MODEL_NOT_ALLOWED`. The typed request structs above have no such
/// field, so this is defense-in-depth for [`Value`] passthroughs. The scan is
/// recursive so a nested `model` key is caught too.
pub fn assert_no_model(body: &Value) -> Result<()> {
    if contains_model_key(body) {
        return Err(
            OrchErrorCode::ModelNotAllowed.to_error("request body must not contain a model field")
        );
    }
    Ok(())
}

fn contains_model_key(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.contains_key("model") || map.values().any(contains_model_key),
        Value::Array(items) => items.iter().any(contains_model_key),
        _ => false,
    }
}

fn take_string(obj: &mut serde_json::Map<String, Value>, key: &str) -> Result<String> {
    match obj.remove(key) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(OpenCompanyError::orchestration(
            OrchErrorCode::ValidationError.as_str(),
            format!("effect frame missing string field `{key}`"),
        )),
    }
}

#[cfg(test)]
mod test;
