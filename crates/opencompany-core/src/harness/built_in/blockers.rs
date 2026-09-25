//! Deciding whether a stop is **answerable by a person** (issue #1861).
//!
//! The settle sites in `brain.rs` and `planning.rs` reach this module holding an
//! error and one question: is this something the operator could fix if we asked
//! them? [`classify_blocker_message`] is the single answer, so the three sites
//! cannot each decide it differently.
//!
//! # Matching on the message, and why that is the only option here
//!
//! A turn returns `anyhow::Result`, so the typed error is erased long before it
//! reaches a settle. Every classifier in this crate that needs to know *what
//! kind* of failure happened therefore matches the flattened error chain —
//! [`is_transient_empty_response`](super::is_transient_empty_response) and
//! [`is_wall_clock_ceiling`](super::is_wall_clock_ceiling) both do — and this
//! follows them rather than inventing a parallel mechanism.
//!
//! The known cost is that a provider's response body reaches the chain
//! verbatim, so a phrase can arrive quoted from a remote service rather than
//! raised locally. That is why the tables below hold **whole phrases** and not
//! single words: matching the bare word "credential" would classify any error
//! whose body happens to mention one.
//!
//! # Being wrong costs different things in each direction
//!
//! A missed blocker settles `Failed` — exactly today's behaviour, surfaced by
//! issue #1865's honest verdicts. A *false* blocker parks work and spends an
//! operator's attention on a question they cannot answer, and it does so
//! silently until the TTL expires it.
//!
//! The asymmetry is why this ships deliberately conservative: a small allowlist
//! of shapes we can name, and `None` — keep settling `Failed` — for everything
//! else. Widening the list later is a diff against a known set. Starting wide
//! and narrowing means walking back parks that already reached people.

use crate::ports::blockers::{BlockerKind, BlockerSource};

/// A recognised stop: what kind of gap it is, where it came from, and what
/// would unblock it.
///
/// The `needed` string is not decoration. A blocker whose payload cannot say
/// what would unstick it arrives as a question with no answerable content —
/// which is a failure wearing a question's clothes, and worse than the plain
/// failure it replaced. Every row below supplies one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockerClass {
    /// What is missing. Decides whether this parks at all.
    pub kind: BlockerKind,
    /// Where the stop came from. Provenance only.
    pub source: BlockerSource,
    /// What would unblock it, in the words a person should read.
    pub needed: &'static str,
}

/// One row of the allowlist: the phrases that identify a shape, and the class
/// they mean.
struct Shape {
    /// Whole phrases, lowercased. Any one matching claims the error.
    leaves: &'static [&'static str],
    class: BlockerClass,
}

/// The shapes we are willing to name, most specific first.
///
/// Order matters: the first match wins, so a phrase that could belong to two
/// rows must sit under the row that describes it better. Rate limiting is
/// listed above the generic auth row for exactly that reason — a 429 body
/// frequently mentions the key it is throttling. MCP-specific errors are listed
/// before generic transient patterns to avoid misclassification of MCP failures
/// as transient stops (issue #1861).
const SHAPES: &[Shape] = &[
    // ---- infrastructure: MCP connections (most specific, checked first) ------
    Shape {
        leaves: &[
            "could not connect to mcp server",
            "mcp server is not connected",
            "connection is not authorised",
            "reconnect the app",
            "oauth token has expired",
            "invalid_grant",
        ],
        class: BlockerClass {
            kind: BlockerKind::Infrastructure,
            source: BlockerSource::Tool,
            needed: "the integration reconnected from Apps",
        },
    },
    // ---- transient: recognised precisely so it does NOT park ----------------
    Shape {
        leaves: &[
            "rate limit",
            "too many requests",
            "429",
            "temporarily unavailable",
            "connection reset",
            "connection refused",
            "timed out",
            "timeout",
        ],
        class: BlockerClass {
            kind: BlockerKind::Transient,
            source: BlockerSource::Provider,
            needed: "nothing — the next attempt may succeed",
        },
    },
    // ---- infrastructure: a person can fix it, but only the operator ---------
    Shape {
        leaves: &[
            "model not found",
            "unknown model",
            "invalid model",
            "does not exist or you do not have access to it",
            "model_not_found",
        ],
        class: BlockerClass {
            kind: BlockerKind::Infrastructure,
            source: BlockerSource::Provider,
            needed: "a model id this provider serves, set on the teammate or the company default",
        },
    },
    Shape {
        leaves: &[
            "invalid api key",
            "incorrect api key",
            "authentication failed",
            "unauthorized",
            "401",
            "invalid_api_key",
        ],
        class: BlockerClass {
            kind: BlockerKind::Infrastructure,
            source: BlockerSource::Provider,
            needed: "a working API key for this provider",
        },
    },
];

/// Classifies a settle-site error message, or `None` when the shape is not one
/// we are willing to name.
///
/// `None` is the ordinary answer and means "settle as before". Callers must not
/// read it as "not a blocker" in any deeper sense — it means this function does
/// not recognise the error, which is a statement about the allowlist and not
/// about the failure.
///
/// Note that a [`BlockerKind::Transient`] result is a *recognised* stop that
/// still must not park; callers gate on
/// [`BlockerKind::parks`](crate::ports::blockers::BlockerKind::parks) rather
/// than on `is_some`.
pub fn classify_blocker_message(message: &str) -> Option<BlockerClass> {
    let haystack = message.to_ascii_lowercase();
    SHAPES
        .iter()
        .find(|shape| {
            shape.leaves.iter().any(|leaf| {
                if BOUNDED_LEAVES.contains(leaf) {
                    contains_bounded(&haystack, leaf)
                } else {
                    haystack.contains(leaf)
                }
            })
        })
        .map(|shape| shape.class)
}

/// The leaves that only count as a match when they stand alone as a word
/// (issue #1861).
///
/// Both are bare status codes, and a bare status code is a substring of longer
/// numbers that mean something else entirely: `port 4010` is not a `401`, and a
/// request id like `req-4290` is not a `429`. Every other leaf is a phrase that
/// cannot collide this way, so it stays a plain `contains`.
///
/// The boundary belongs here and **not** in the leaf. `401` was once spelled as
/// the pair `"401 "` / `" 401"`, which is a boundary check written in the wrong
/// place and does not work: the leading-space form put the space *inside* the
/// leaf, so the start-boundary test read the character before the space — a
/// letter in every real message — and rejected every one of them. The
/// trailing-space form then missed `401:` and a line ending in `401`. Both
/// leaves were dead, and the auth row only ever matched through its prose
/// phrases.
const BOUNDED_LEAVES: &[&str] = &["401", "429"];

/// Whether `leaf` occurs in `haystack` with a non-alphanumeric character (or
/// the string's edge) on both sides.
///
/// **Every occurrence, not the first.** Checking only the first one is how
/// `request req-4290 received http 429` used to read as unrecognised: the scan
/// stopped at the `429` inside `4290`, rejected its trailing `0`, and never
/// reached the real status code later in the line. A missed transient shape is
/// not a harmless miss — the row below it is the auth row, so a throttled
/// request whose body also names a key would park as a credential problem and
/// wait for a person who has nothing to fix.
fn contains_bounded(haystack: &str, leaf: &str) -> bool {
    haystack.match_indices(leaf).any(|(pos, _)| {
        let starts_clean =
            pos == 0 || haystack[..pos].ends_with(|c: char| !c.is_ascii_alphanumeric());
        let end = pos + leaf.len();
        let ends_clean = end >= haystack.len()
            || haystack[end..].starts_with(|c: char| !c.is_ascii_alphanumeric());
        starts_clean && ends_clean
    })
}

/// The connection a stop is about, when the reason names one — the key that
/// folds ten cards stalled on one broken integration into a single question
/// (issue #1862).
///
/// Only the recognised connection shapes carry a groupable identity: the phrase
/// that identified the stop as an integration failure sits beside a backticked
/// name (`could not connect to mcp server \`slack\``), and that name is the
/// connection. Everything else returns `None` and groups with nothing — the
/// same conservative default the classifier itself keeps, for the same reason:
/// a wrong group merges two unrelated questions into one card, which is worse
/// than two honest cards.
///
/// Deliberately not a general backtick scan: an auth or model-id reason also
/// carries a backticked token (the rejected model, a key label), and grouping
/// those would collapse distinct provider stops. The gate is that the reason
/// matched an integration-connection shape in the first place.
pub fn connection_group_key(reason: &str) -> Option<String> {
    let lower = reason.to_ascii_lowercase();
    const CONNECTION_MARKERS: &[&str] = &[
        "could not connect to mcp server",
        "mcp server is not connected",
        "connection is not authorised",
        "reconnect the app",
    ];
    let marker_end = CONNECTION_MARKERS
        .iter()
        .filter_map(|m| lower.find(m).map(|pos| pos + m.len()))
        .min()?;
    let name = reason[marker_end..]
        .split('`')
        .nth(1)
        .map(str::trim)
        .filter(|name| !name.is_empty())?;
    Some(format!("connection:{name}"))
}

/// The class a planning pass's missing prerequisite parks as.
///
/// Not message-matched: `settle_blocked` already *knows* the card cannot
/// proceed and why — the pass told it — so there is nothing to infer. Naming it
/// here rather than inline keeps every blocker class in one file.
pub const PREREQ_BLOCKER: BlockerClass = BlockerClass {
    kind: BlockerKind::Information,
    source: BlockerSource::Prereq,
    needed: "the missing prerequisite, or a decision to proceed without it",
};

/// The class an agent's own `escalate_to_human` parks as.
///
/// Always [`Information`](BlockerKind::Information): the agent is asking a
/// question, and the answer is knowledge it does not have. An agent that
/// escalates a broken integration is still asking a person to supply
/// something — the routing is the same, and #1866 is where a smarter reading of
/// the question belongs.
pub const AGENT_QUESTION_BLOCKER: BlockerClass = BlockerClass {
    kind: BlockerKind::Information,
    source: BlockerSource::AgentQuestion,
    needed: "an answer to the question on this card",
};

// ───────────────────────────────────────────────────────────────────────────
// The agent's own door (issue #1861)
// ───────────────────────────────────────────────────────────────────────────

/// The `escalate_to_human` tool name.
pub const ESCALATE_TO_HUMAN_TOOL: &str = "escalate_to_human";

/// Queues an [`Information`](BlockerKind::Information) blocker for the operator.
/// The turn's drain parks accepted questions through the approval lifecycle.
///
/// Escalation establishes the turn boundary: subsequent tool calls are refused
/// until a new turn. An accepted question is queued for the operator; a full
/// batch returns an explicit refusal.
pub struct EscalateToHumanTool {
    requests: crate::harness::built_in::policy::ApprovalRequestQueue,
    agent_id: String,
    agent_label: String,
}

impl EscalateToHumanTool {
    /// Builds the tool over the shared approval-request queue, for one agent.
    ///
    /// `agent_label` is the name a person reads (the roster display name, or
    /// the role when there is none) — never the roster id itself, which
    /// `agent_id` carries separately for the card's "Asked by" attribution.
    pub fn new(
        requests: crate::harness::built_in::policy::ApprovalRequestQueue,
        agent_id: String,
        agent_label: String,
    ) -> Self {
        Self {
            requests,
            agent_id,
            agent_label,
        }
    }
}

#[async_trait::async_trait]
impl tinytools::Tool for EscalateToHumanTool {
    fn name(&self) -> &str {
        ESCALATE_TO_HUMAN_TOOL
    }

    fn description(&self) -> &str {
        "Ask the operator a question you cannot answer yourself, when the work genuinely cannot \
         proceed without it — a missing prerequisite, a choice only they can make, two \
         instructions that contradict each other. Provide the `question` in plain words, and \
         optionally the `context` you already gathered. The card parks and waits for their \
         answer rather than failing. Use it instead of guessing, and instead of finishing with \
         prose explaining that you were stuck. Do NOT use it for something you can look up, for \
         something a teammate would know, or to confirm a decision you have already been given."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "What you need the operator to tell you, in one or two plain sentences."
                },
                "context": {
                    "type": "string",
                    "description": "Optional: what you already tried or found, so they can answer without re-deriving it."
                }
            },
            "required": ["question"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> tinytools::PermissionLevel {
        tinytools::PermissionLevel::Write
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<tinytools::ToolResult> {
        use tinytools::ToolResult;

        let question = args
            .get("question")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`question` is required"))?
            .to_string();
        let context = args
            .get("context")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty());

        // A person reading this card by role or name, never by roster id —
        // falling back to a generic label rather than ever printing the id.
        let trimmed_label = self.agent_label.trim();
        let asker_label = if trimmed_label.is_empty() {
            "a teammate"
        } else {
            trimmed_label
        };

        // The reason a person reads is the question plus whatever the agent
        // already worked out — not a wrapper sentence about escalation, which
        // would push the actual question down the card.
        let reason = match context {
            Some(context) => format!("{question}\n\nWhat {asker_label} already has: {context}"),
            None => question.clone(),
        };

        let payload = crate::ports::blockers::BlockerPayload {
            kind: AGENT_QUESTION_BLOCKER.kind,
            source: AGENT_QUESTION_BLOCKER.source,
            // No step: a question asked mid-conversation has no card behind it,
            // and where one does exist the approval's own task link already
            // names it. See `BlockerPayload::step`.
            step: None,
            reason: reason.clone(),
            needed: AGENT_QUESTION_BLOCKER.needed.to_string(),
            // A question is particular to its own card; nothing else shares its
            // answer, so it groups with nothing.
            group_key: None,
        };
        let mut payload_json = serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
        // The one blocker shape with an unambiguous asker: an additive JSON
        // key rather than a `BlockerPayload` field, so `crate::ports::blockers::asked_by`
        // can hand the console an "Asked by" name without every other blocker
        // construction site having to grow a field it would only ever set to
        // `None`. See that function's doc comment.
        if let serde_json::Value::Object(fields) = &mut payload_json {
            fields.insert(
                "asked_by".to_string(),
                serde_json::Value::String(self.agent_id.clone()),
            );
        }
        let effect = crate::ports::types::Effect {
            kind: payload.effect_kind(),
            group: crate::ports::types::EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: payload_json,
            // `None`, even though an agent did raise this and the field exists
            // to name one. `Some(agent)` means "a tool call openhuman blocked",
            // and approving one mints a single-use grant and re-dispatches the
            // agent to run that exact call again — which here would call
            // `escalate_to_human` a second time and park the same question.
            // Carrying the operator's answer back into the turn is #1863; until
            // it lands, approving a blocker is deliberately inert. The asking
            // agent still reaches the card through the payload's `asked_by`
            // key, which carries no such re-dispatch meaning.
            agent: None,
            // Stamped by the dispatch boundary's `stamp_run`, which retro-fills
            // every request this turn queued.
            run_id: None,
        };
        use crate::harness::built_in::policy::ApprovalPush;
        match self
            .requests
            .push_blocker(crate::harness::built_in::policy::ApprovalRequest {
                tool: ESCALATE_TO_HUMAN_TOOL.to_string(),
                reason,
                effect,
            }) {
            ApprovalPush::Queued => {}
            ApprovalPush::OverCap => {
                return Ok(ToolResult::error(format!(
                    "Your question was not raised: this batch already has the maximum of {} \
                     approval requests. Stop and wait for the queued requests to be resolved, then \
                     ask again.",
                    crate::harness::built_in::policy::MAX_APPROVAL_REQUESTS_PER_TURN
                )));
            }
            ApprovalPush::Unclaimed => {
                return Ok(ToolResult::error(
                    crate::harness::approval_tool::NOT_RECORDED.to_string(),
                ));
            }
        }

        Ok(ToolResult::success(format!(
            "Raised your question with the operator: \"{question}\". This card parks until they \
             answer. Stop and wait for their answer; do not ask it again."
        )))
    }
}

#[cfg(test)]
#[path = "blockers_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "blockers_tool_test_tests.rs"]
mod tool_test;
