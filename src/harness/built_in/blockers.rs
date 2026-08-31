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
}

// ───────────────────────────────────────────────────────────────────────────
// The agent's own door (issue #1861)
// ───────────────────────────────────────────────────────────────────────────

/// The `escalate_to_human` tool name.
pub const ESCALATE_TO_HUMAN_TOOL: &str = "escalate_to_human";

/// Lets an agent stop and **ask**, instead of guessing or going quiet.
///
/// Before this, a teammate that hit real ambiguity — "staging or prod?", "which
/// of these two contradictory briefs is current?" — had two options and both
/// were bad: pick one and be silently wrong, or produce prose explaining that
/// it could not proceed, which reads as a completed turn. The orchestrator
/// brief even pointed at `spawn_task` for "work waiting on a person", which
/// opens a card that notifies nobody and resumes nothing.
///
/// This is the third option. The question parks as a durable
/// [`Information`](BlockerKind::Information) blocker on the operator's queue,
/// through the same path a gated tool call parks on, so it survives a restart
/// and expires through the approval TTL rather than waiting forever.
///
/// # Why it stages rather than parks directly
///
/// The tool has no host handle — tools receive arguments and nothing else — so
/// it pushes onto the shared [`ApprovalRequestQueue`] exactly as the approval
/// policy does for a gated call, and the turn's drain parks it. Both drains
/// exist: a chat or task turn drains through `park_approval_requests`, a
/// workflow agent node through `park_gated_calls`. There is no path on which an
/// agent can raise a question that nothing will deliver.
///
/// # What it deliberately does not do
///
/// It does not end the turn. The agent asks and keeps working with what it has;
/// the *run* is what parks, because a turn that queued a blocker settles
/// [`Blocked`](crate::ports::runs::RunStatus::Blocked) rather than reporting a
/// result nobody has confirmed. Ending the turn from inside a tool would
/// discard whatever the agent had already produced, which is the opposite of
/// what a question is for.
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
impl openhuman_core::openhuman::tools::traits::Tool for EscalateToHumanTool {
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

    fn permission_level(&self) -> openhuman_core::openhuman::tools::traits::PermissionLevel {
        openhuman_core::openhuman::tools::traits::PermissionLevel::Write
    }

    async fn execute(
        &self,
        args: serde_json::Value,
    ) -> anyhow::Result<openhuman_core::openhuman::tools::traits::ToolResult> {
        use openhuman_core::openhuman::tools::traits::ToolResult;

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
        self.requests
            .push(crate::harness::built_in::policy::ApprovalRequest {
                tool: ESCALATE_TO_HUMAN_TOOL.to_string(),
                reason,
                effect,
            });

        Ok(ToolResult::success(format!(
            "Raised your question with the operator: \"{question}\". This card parks until they \
             answer, so do not ask it again — carry on with anything else you can do without it."
        )))
    }
}

#[cfg(test)]
mod tool_test {
    use super::*;
    use crate::harness::built_in::policy::ApprovalRequestQueue;
    use crate::ports::blockers::{BlockerKind, BlockerPayload, BlockerSource};
    use openhuman_core::openhuman::tools::traits::Tool;

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
}
