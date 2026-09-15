//! Deciding whether a stop is **answerable by a person** (issue #1861).
//!
//! The settle sites in `brain.rs` and `planning.rs` reach this module holding an
//! error and one question: is this something the operator could fix if we asked
//! them? [`classify_blocker`] is the single answer, so the three sites cannot
//! each decide it differently.
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

/// Classifies a settle-site error, or `None` when the shape is not one we are
/// willing to name.
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
pub fn classify_blocker(err: &anyhow::Error) -> Option<BlockerClass> {
    classify_blocker_message(&format!("{err:#}"))
}

/// [`classify_blocker`] over an already-flattened message.
///
/// Split out because two callers have a `String` rather than an
/// `anyhow::Error`: `planning.rs`'s `settle_blocked`, whose reason is composed
/// rather than raised, and the tests below.
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

#[cfg(test)]
mod test {
    use super::*;

    fn class_of(message: &str) -> Option<BlockerClass> {
        classify_blocker_message(message)
    }

    #[test]
    fn a_rejected_model_id_is_infrastructure() {
        let class = class_of("dispatch failed: the model `gpt-nonexistent` does not exist or you do not have access to it")
            .expect("a rejected model id is recognised");
        assert_eq!(class.kind, BlockerKind::Infrastructure);
        assert_eq!(class.source, BlockerSource::Provider);
        assert!(class.kind.parks());
    }

    #[test]
    fn a_bad_key_is_infrastructure() {
        let class = class_of("hosted inference returned 401: invalid api key")
            .expect("an auth failure is recognised");
        assert_eq!(class.kind, BlockerKind::Infrastructure);
        assert_eq!(class.source, BlockerSource::Provider);
    }

    #[test]
    fn a_disconnected_integration_is_infrastructure_from_a_tool() {
        let class = class_of("tool call failed: could not connect to mcp server `slack`")
            .expect("an MCP connection failure is recognised");
        assert_eq!(class.kind, BlockerKind::Infrastructure);
        assert_eq!(class.source, BlockerSource::Tool);
    }

    /// The point of carrying `Transient` in the taxonomy: it is recognised, and
    /// recognising it is how we know **not** to ask anybody.
    #[test]
    fn a_rate_limit_is_recognised_but_does_not_park() {
        let class = class_of("hosted inference returned 429: rate limit exceeded")
            .expect("a rate limit is recognised");
        assert_eq!(class.kind, BlockerKind::Transient);
        assert!(
            !class.kind.parks(),
            "a rate limit resolves itself; asking a person about it wastes their attention"
        );
    }

    /// Rate limiting outranks the auth row, because a 429 body routinely names
    /// the key it is throttling. Getting this backwards would park a
    /// self-resolving stop as a broken credential.
    #[test]
    fn a_throttled_key_reads_as_transient_not_as_bad_auth() {
        let class = class_of("429 too many requests for this api key").expect("recognised");
        assert_eq!(class.kind, BlockerKind::Transient);
    }

    /// The boundary check reads every occurrence, not just the first.
    ///
    /// A provider line that names a request id before its status code —
    /// `req-4290 … http 429` — used to stop at the `429` inside `4290`, reject
    /// its trailing `0`, and never reach the real code. The miss was not
    /// neutral: the auth row sits below the transient one, so the same body
    /// mentioning a key would then park a self-resolving throttle as a broken
    /// credential and wait for a person with nothing to fix.
    #[test]
    fn a_status_code_is_found_past_an_earlier_unbounded_lookalike() {
        let class = class_of("request req-4290 received HTTP 429").expect("recognised");
        assert_eq!(class.kind, BlockerKind::Transient);

        let auth_flavoured =
            class_of("request req-4290 failed: http 429 for this api key").expect("recognised");
        assert_eq!(
            auth_flavoured.kind,
            BlockerKind::Transient,
            "the real 429 must still outrank the auth row it is quoted beside"
        );
    }

    /// …and the boundary itself still holds: a longer number that merely starts
    /// with a status code is not that status code.
    #[test]
    fn a_longer_number_is_not_a_status_code() {
        assert_eq!(class_of("dispatch failed on port 4010"), None);
        assert_eq!(class_of("dispatch failed: worker 4290 died"), None);
    }

    /// A bare `401` is enough on its own, whatever punctuation follows it.
    ///
    /// It was not, and nothing said so: the leaf was spelled as the pair
    /// `"401 "` / `" 401"` — a boundary check written into the leaf instead of
    /// around it. The leading-space form made the start-boundary test read the
    /// character *before* the space, a letter in every real provider message,
    /// so it rejected all of them; the trailing-space form missed `401:` and a
    /// line ending in `401`. Every existing 401 test passed anyway, because
    /// each message also carried `invalid api key` or `unauthorized` — the
    /// prose phrases were doing all the work and the status code none of it.
    #[test]
    fn a_bare_401_is_an_auth_blocker_whatever_follows_it() {
        for message in [
            "hosted inference returned 401: invalid credentials",
            "provider rejected the call with http 401",
            "401 returned by the upstream",
        ] {
            let class = class_of(message).unwrap_or_else(|| panic!("unrecognised: {message}"));
            assert_eq!(
                class.kind,
                BlockerKind::Infrastructure,
                "message: {message}"
            );
            assert_eq!(class.source, BlockerSource::Provider, "message: {message}");
        }
    }

    /// The conservative default. An error we cannot name keeps today's
    /// behaviour rather than guessing at a question for somebody.
    #[test]
    fn an_unrecognised_failure_is_not_a_blocker() {
        assert_eq!(class_of("dispatch failed: index out of bounds"), None);
        assert_eq!(
            class_of("hand-off failed: the delegate produced nothing"),
            None
        );
        assert_eq!(class_of(""), None);
    }

    /// Whole phrases, not loose words — a provider body that merely mentions a
    /// credential must not be read as a broken one.
    #[test]
    fn a_body_that_merely_mentions_a_key_is_not_an_auth_blocker() {
        assert_eq!(
            class_of("the document describes how to store an api key safely"),
            None,
            "matching the bare word `api key` would park an unrelated failure"
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(class_of("HTTP 401 UNAUTHORIZED").is_some());
    }

    /// Every row promises something a person can act on, including the
    /// transient row (whose promise is that there is nothing to do).
    #[test]
    fn every_shape_says_what_is_needed() {
        for shape in SHAPES {
            assert!(
                !shape.class.needed.trim().is_empty(),
                "a blocker with nothing in `needed` reaches a person with nothing to do"
            );
            assert!(
                !shape.leaves.is_empty(),
                "a shape with no phrases can never match"
            );
        }
        assert!(!PREREQ_BLOCKER.needed.is_empty());
        assert!(!AGENT_QUESTION_BLOCKER.needed.is_empty());
    }

    /// The two host-declared classes park by construction — they exist because
    /// something already established a person is needed.
    #[test]
    fn declared_classes_park() {
        assert!(PREREQ_BLOCKER.kind.parks());
        assert!(AGENT_QUESTION_BLOCKER.kind.parks());
    }

    /// Two cards stalled on the same integration carry the same group key, so
    /// the console folds them into one question instead of two.
    #[test]
    fn a_named_connection_groups_by_its_name() {
        let a = connection_group_key("could not connect to mcp server `slack`")
            .expect("a named connection groups");
        let b = connection_group_key("dispatch failed: could not connect to mcp server `slack`")
            .expect("the same connection, a different reason");
        assert_eq!(a, b);
        assert_eq!(a, "connection:slack");
        assert_ne!(
            connection_group_key("could not connect to mcp server `notion`"),
            Some(a),
            "a different server is a different question"
        );
    }

    /// The gate is that the reason matched a connection shape — a backticked
    /// token in an auth or model-id reason is not a connection and must not
    /// group those distinct stops together.
    #[test]
    fn a_backtick_outside_a_connection_shape_does_not_group() {
        assert_eq!(connection_group_key("unknown model `gpt-nope`"), None);
        assert_eq!(connection_group_key("invalid api key `sk-123`"), None);
        assert_eq!(
            connection_group_key("could not connect to mcp server"),
            None,
            "a connection shape with no name has nothing to group by"
        );
    }

    /// A backticked token before the connection marker — a tool name in the
    /// prose — is not the connection; the name after the marker is.
    #[test]
    fn the_name_is_read_after_the_connection_marker() {
        assert_eq!(
            connection_group_key("tool `search` failed: could not connect to mcp server `slack`"),
            Some("connection:slack".to_string()),
            "the connection is the server after the marker, not the earlier tool token"
        );
    }
}

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
    agent: String,
}

impl EscalateToHumanTool {
    /// Builds the tool over the shared approval-request queue, for one agent.
    pub fn new(
        requests: crate::harness::built_in::policy::ApprovalRequestQueue,
        agent: String,
    ) -> Self {
        Self { requests, agent }
    }
}

#[async_trait::async_trait]
impl openhuman_core::tools::traits::Tool for EscalateToHumanTool {
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

    fn permission_level(&self) -> openhuman_core::tools::traits::PermissionLevel {
        openhuman_core::tools::traits::PermissionLevel::Write
    }

    async fn execute(
        &self,
        args: serde_json::Value,
    ) -> anyhow::Result<openhuman_core::tools::traits::ToolResult> {
        use openhuman_core::tools::traits::ToolResult;

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

        // The reason a person reads is the question plus whatever the agent
        // already worked out — not a wrapper sentence about escalation, which
        // would push the actual question down the card.
        let reason = match context {
            Some(context) => format!("{question}\n\nWhat {} already has: {context}", self.agent),
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
        let effect = crate::ports::types::Effect {
            kind: payload.effect_kind(),
            group: crate::ports::types::EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null),
            // `None`, even though an agent did raise this and the field exists
            // to name one. `Some(agent)` means "a tool call openhuman blocked",
            // and approving one mints a single-use grant and re-dispatches the
            // agent to run that exact call again — which here would call
            // `escalate_to_human` a second time and park the same question.
            // Carrying the operator's answer back into the turn is #1863; until
            // it lands, approving a blocker is deliberately inert.
            agent: None,
            // Stamped by the dispatch boundary's `stamp_run`, which retro-fills
            // every request this turn queued.
            run_id: None,
        };
        if !self
            .requests
            .push_blocker(crate::harness::built_in::policy::ApprovalRequest {
                tool: ESCALATE_TO_HUMAN_TOOL.to_string(),
                reason,
                effect,
            })
        {
            return Ok(ToolResult::error(format!(
                "Your question was not raised: this batch already has the maximum of {} approval \
                 requests. Stop and wait for the queued requests to be resolved, then ask again.",
                crate::harness::built_in::policy::MAX_APPROVAL_REQUESTS_PER_TURN
            )));
        }

        Ok(ToolResult::success(format!(
            "Raised your question with the operator: \"{question}\". This card parks until they \
             answer. Stop and wait for their answer; do not ask it again."
        )))
    }
}

#[cfg(test)]
mod tool_test {
    use super::*;
    use crate::harness::built_in::policy::ApprovalRequestQueue;
    use crate::ports::blockers::{BlockerKind, BlockerPayload, BlockerSource};
    use openhuman_core::tools::traits::Tool;

    fn tool(queue: &ApprovalRequestQueue) -> EscalateToHumanTool {
        EscalateToHumanTool::new(queue.clone(), "engineer".to_string())
    }

    #[tokio::test]
    async fn a_question_parks_as_an_information_blocker() {
        let queue = ApprovalRequestQueue::default();
        let result = tool(&queue)
            .execute(serde_json::json!({ "question": "staging or prod?" }))
            .await
            .expect("the tool runs");
        assert!(
            !result.is_error,
            "asking is not a failure: {}",
            result.text()
        );

        let drained = queue.drain(8);
        assert_eq!(drained.requests.len(), 1);
        let request = &drained.requests[0];
        assert_eq!(request.tool, ESCALATE_TO_HUMAN_TOOL);
        assert_eq!(request.effect.kind, "blocker.information");

        let payload: BlockerPayload =
            serde_json::from_value(request.effect.payload.clone()).expect("payload round-trips");
        assert_eq!(payload.kind, BlockerKind::Information);
        assert_eq!(payload.source, BlockerSource::AgentQuestion);
        assert_eq!(
            payload.step, None,
            "a question asked mid-turn names no step; the approval's task link does"
        );
        assert!(payload.reason.contains("staging or prod?"));
    }

    /// The context the agent already gathered rides along, so the operator can
    /// answer without re-deriving it — and it is joined into the reason rather
    /// than dropped into a field nothing renders yet.
    #[tokio::test]
    async fn gathered_context_reaches_the_question() {
        let queue = ApprovalRequestQueue::default();
        tool(&queue)
            .execute(serde_json::json!({
                "question": "which brief is current?",
                "context": "the Jan and Mar briefs contradict on pricing"
            }))
            .await
            .expect("the tool runs");

        let drained = queue.drain(8);
        let payload: BlockerPayload =
            serde_json::from_value(drained.requests[0].effect.payload.clone()).expect("payload");
        assert!(payload.reason.contains("which brief is current?"));
        assert!(payload.reason.contains("contradict on pricing"));
        assert!(
            payload.reason.contains("engineer"),
            "the context is attributed to the agent that gathered it"
        );
    }

    /// A blank question is refused rather than parked: an empty card reaches a
    /// person with nothing to answer and still costs them the interruption.
    #[tokio::test]
    async fn an_empty_question_is_refused_and_parks_nothing() {
        let queue = ApprovalRequestQueue::default();
        assert!(
            tool(&queue)
                .execute(serde_json::json!({ "question": "   " }))
                .await
                .is_err()
        );
        assert!(queue.drain(8).requests.is_empty());
    }

    /// Approving an escalation must not re-dispatch the agent into calling the
    /// same tool again — see the `agent` field's note.
    #[tokio::test]
    async fn an_escalation_mints_no_grant() {
        let queue = ApprovalRequestQueue::default();
        tool(&queue)
            .execute(serde_json::json!({ "question": "staging or prod?" }))
            .await
            .expect("runs");
        assert!(queue.drain(8).requests[0].effect.agent.is_none());
    }

    /// Two agents asking distinct questions in the same turn race through
    /// `execute` concurrently — nothing upstream of this tool serialises the
    /// calls — so both must still land their own card rather than one
    /// silently losing to the other on the shared queue's `Mutex`.
    ///
    /// Driven from two worker threads through a [`Barrier`], not from
    /// `tokio::join!`: `execute` has no suspension point around its
    /// synchronous `push`, so joined futures are polled to completion one
    /// after the other on a single task. That arrangement exercises two serial
    /// inserts and would pass unchanged if simultaneous calls could lose a
    /// card — which is the only thing this test exists to rule out.
    ///
    /// Repeated, because the barrier releases both workers before either
    /// reaches `push` rather than at `push` itself: a single round can
    /// interleave benignly. Making the window certain would mean a test hook
    /// inside the queue every caller pays for, so the rounds buy it instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_questions_from_different_agents_both_park() {
        use std::sync::{Arc, Barrier};

        for round in 0..20 {
            let queue = ApprovalRequestQueue::default();
            let finance = EscalateToHumanTool::new(queue.clone(), "finance".to_string());
            let legal = EscalateToHumanTool::new(queue.clone(), "legal".to_string());
            let gate = Arc::new(Barrier::new(2));

            let ask = |tool: EscalateToHumanTool, question: &'static str, gate: Arc<Barrier>| {
                tokio::task::spawn_blocking(move || {
                    gate.wait();
                    tokio::runtime::Handle::current()
                        .block_on(tool.execute(serde_json::json!({ "question": question })))
                })
            };
            let a = ask(finance, "approve the Q3 budget?", gate.clone());
            let b = ask(legal, "sign the NDA as-is?", gate.clone());
            assert!(!a.await.expect("joins").expect("runs").is_error);
            assert!(!b.await.expect("joins").expect("runs").is_error);

            let drained = queue.drain(8);
            let reasons: Vec<&String> = drained.requests.iter().map(|r| &r.reason).collect();
            assert_eq!(
                drained.requests.len(),
                2,
                "round {round}: both concurrent questions must reach the queue, not just \
                 whichever wins the race: {reasons:?}"
            );
            for question in ["approve the Q3 budget?", "sign the NDA as-is?"] {
                assert!(
                    reasons.iter().any(|reason| reason.contains(question)),
                    "round {round}: the queue must hold each agent's own question, not one of \
                     them twice: {reasons:?}"
                );
            }
        }
    }

    /// The other half of `an_escalation_mints_no_grant`, and the half that is
    /// load-bearing in the opposite direction.
    ///
    /// `agent: None` is what stops an approval re-dispatching the agent into
    /// asking the same question again. But `None` is also what
    /// `CycleRunner::settle_approval` reads as *a native effect the runtime
    /// performs*, and its fall-through hands the effect to
    /// `execute_effect_once` — which for a blocker payload ledgers a phantom
    /// spend and routes nothing while reporting success. The only thing
    /// standing between those two is
    /// [`is_blocker_effect`](crate::ports::blockers::is_blocker_effect), which
    /// matches on the effect **kind string**. Nothing else couples the kind
    /// this tool stamps to the prefix that guard looks for, so a rename on
    /// either side reopens the fall-through silently.
    #[tokio::test]
    async fn an_escalation_is_recognisable_as_a_blocker_so_approval_cannot_execute_it_natively() {
        let queue = ApprovalRequestQueue::default();
        tool(&queue)
            .execute(serde_json::json!({ "question": "staging or prod?" }))
            .await
            .expect("runs");
        let effect = queue.drain(8).requests[0].effect.clone();
        assert!(
            effect.agent.is_none(),
            "a grant here would re-ask the question"
        );
        assert!(
            crate::ports::blockers::is_blocker_effect(&effect),
            "an agent-None effect that is not recognised as a blocker falls through to native \
             execution on approval: {}",
            effect.kind
        );
        assert!(
            serde_json::from_value::<BlockerPayload>(effect.payload.clone()).is_ok(),
            "the resolve path reads the payload back off the parked effect to carry the step: {:?}",
            effect.payload
        );
        assert!(
            effect.amount_usd.is_none(),
            "a question costs nothing; an amount here is what a phantom spend would be ledgered \
             from"
        );
    }

    /// STATE-axis (REQ-002): `push`'s de-duplication is per [`ApprovalScope`]
    /// (issue #439) — two different turns asking the identical question are
    /// two requests, not one collapsed into the other. This is the flip side
    /// of `a_repeated_identical_escalation_collapses_but_a_distinct_one_survives`
    /// (`policy.rs`), which proves the collapse WITHIN one turn; this proves a
    /// prior turn's already-drained card does not leave state that suppresses
    /// an identical question asked again in a later, separate turn.
    #[tokio::test]
    async fn escalate_to_human_repeated_across_different_turns_is_not_deduped() {
        let queue = ApprovalRequestQueue::default();
        let tool = tool(&queue);

        let first_turn = queue.claim(crate::harness::built_in::policy::ApprovalScope::Run(
            "run-1".to_string(),
        ));
        let first_drain = first_turn
            .scoped(async {
                tool.execute(serde_json::json!({ "question": "staging or prod?" }))
                    .await
                    .expect("first turn runs");
                queue.drain(8)
            })
            .await;
        assert_eq!(
            first_drain.requests.len(),
            1,
            "the first turn's own question lands"
        );
        drop(first_turn);

        let second_turn = queue.claim(crate::harness::built_in::policy::ApprovalScope::Run(
            "run-2".to_string(),
        ));
        let second_drain = second_turn
            .scoped(async {
                tool.execute(serde_json::json!({ "question": "staging or prod?" }))
                    .await
                    .expect("second turn runs");
                queue.drain(8)
            })
            .await;
        assert_eq!(
            second_drain.requests.len(),
            1,
            "a later, separate turn asking the identical question must not read as a duplicate \
             of a card the first turn already drained and lost scope of"
        );
    }

    #[tokio::test]
    async fn escalate_to_human_exactly_at_the_cap_produces_no_overflow() {
        use crate::harness::built_in::policy::MAX_APPROVAL_REQUESTS_PER_TURN;

        let queue = ApprovalRequestQueue::default();
        let tool = tool(&queue);
        for i in 0..MAX_APPROVAL_REQUESTS_PER_TURN {
            let outcome = tool
                .execute(serde_json::json!({ "question": format!("question {i}?") }))
                .await
                .expect("runs");
            assert!(!outcome.is_error, "question {i}: {}", outcome.text());
        }

        let drained = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(
            drained.requests.len(),
            MAX_APPROVAL_REQUESTS_PER_TURN,
            "exactly the cap's worth of distinct questions must all land"
        );
        assert_eq!(
            drained.discarded, 0,
            "at exactly the cap, nothing overflows"
        );
        assert!(
            drained.overflow_notice().is_none(),
            "no notice is owed when nothing was dropped"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_questions_compete_for_the_final_slot_without_silent_loss() {
        use crate::harness::built_in::policy::MAX_APPROVAL_REQUESTS_PER_TURN;
        use std::sync::{Arc, Barrier};

        for round in 0..20 {
            let queue = ApprovalRequestQueue::default();
            for i in 0..MAX_APPROVAL_REQUESTS_PER_TURN - 1 {
                let asked = tool(&queue)
                    .execute(serde_json::json!({ "question": format!("existing question {i}") }))
                    .await
                    .expect("the tool runs");
                assert!(!asked.is_error, "{}", asked.text());
            }

            let barrier = Arc::new(Barrier::new(2));
            let ask = |agent: &str, question: &'static str| {
                let tool = EscalateToHumanTool::new(queue.clone(), agent.to_string());
                let barrier = barrier.clone();
                tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    tokio::runtime::Handle::current()
                        .block_on(tool.execute(serde_json::json!({ "question": question })))
                })
            };
            let finance = ask("finance", "approve the final budget?");
            let legal = ask("legal", "approve the final contract?");
            let results = [
                (
                    "approve the final budget?",
                    finance.await.expect("joins").expect("the tool runs"),
                ),
                (
                    "approve the final contract?",
                    legal.await.expect("joins").expect("the tool runs"),
                ),
            ];
            assert_eq!(
                results
                    .iter()
                    .filter(|(_, result)| !result.is_error)
                    .count(),
                1,
                "round {round}: one remaining blocker slot must have exactly one successful caller"
            );

            let drained = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
            assert_eq!(drained.requests.len(), MAX_APPROVAL_REQUESTS_PER_TURN);
            assert_eq!(
                drained.discarded, 0,
                "no accepted question may be discarded"
            );
            for i in 0..MAX_APPROVAL_REQUESTS_PER_TURN - 1 {
                assert!(
                    drained
                        .requests
                        .iter()
                        .any(|r| r.reason == format!("existing question {i}"))
                );
            }
            for (question, result) in results {
                let retained = drained.requests.iter().any(|r| r.reason == question);
                assert_eq!(retained, !result.is_error, "round {round}: {question}");
                if result.is_error {
                    assert!(result.text().contains("not raised"), "{}", result.text());
                }
            }
        }
    }

    #[tokio::test]
    async fn a_full_run_accepts_its_duplicate_without_consuming_another_runs_capacity() {
        use crate::harness::built_in::policy::{ApprovalScope, MAX_APPROVAL_REQUESTS_PER_TURN};

        let queue = ApprovalRequestQueue::default();
        let full = queue.claim(ApprovalScope::Run("full".to_string()));
        let other = queue.claim(ApprovalScope::Run("other".to_string()));
        let tool = tool(&queue);
        full.scoped(async {
            for i in 0..MAX_APPROVAL_REQUESTS_PER_TURN {
                let asked = tool
                    .execute(serde_json::json!({ "question": format!("question {i}") }))
                    .await
                    .expect("the tool runs");
                assert!(!asked.is_error, "{}", asked.text());
            }
            let duplicate = tool
                .execute(serde_json::json!({ "question": "question 0" }))
                .await
                .expect("the tool runs");
            assert!(
                !duplicate.is_error,
                "the existing question is already queued"
            );
            let refused = tool
                .execute(serde_json::json!({ "question": "new question" }))
                .await
                .expect("the tool runs");
            assert!(
                refused.is_error,
                "a new question must be refused at the cap"
            );
        })
        .await;

        let independent = other
            .scoped(tool.execute(serde_json::json!({ "question": "question 0" })))
            .await
            .expect("the tool runs");
        assert!(
            !independent.is_error,
            "a different run has its own capacity"
        );
        let full_drain = full
            .scoped(async { queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN) })
            .await;
        assert_eq!(full_drain.requests.len(), MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(full_drain.discarded, 0);
        let other_drain = other
            .scoped(async { queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN) })
            .await;
        assert_eq!(other_drain.requests.len(), 1);
        assert_eq!(other_drain.requests[0].reason, "question 0");
        assert_eq!(other_drain.discarded, 0);
    }
}
