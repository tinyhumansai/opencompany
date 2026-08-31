//! The LLM triage escalation — a second opinion on the operator messages the
//! lexical classifier could not name (issue #678).
//!
//! # Escalation, not a pre-classifier
//!
//! Issue #267 sketches an LLM classifier fronting operator chat, and #678 asks
//! for it. Read literally that is a full-price, serial round-trip **before every
//! reply**, which is a poor trade: [`triage_message`] already names the classes
//! a lexical detector can name, conservatively, and it is free.
//!
//! What it cannot do is tell you when it is guessing — every residual falls into
//! [`MessageTriage::Chatter`] by construction. That is what
//! [`triage_message_detailed`] fixed: a `Chatter` the classifier *decided*
//! (empty, a greeting) is now distinguishable from one it *fell back to*.
//!
//! So this runs only on an abstention. Messages the cheap layer already named
//! cost nothing and wait for nothing, and the model is asked only about the
//! residue — the difference between paying per hard message and paying per
//! message.
//!
//! # It decides, it does not act
//!
//! Lifted from OpenHuman's `trigger_triage`, whose framing this keeps verbatim:
//! *"You do not act. You decide. Another component will carry out your
//! decision."* The request carries **no tools at all**, so there is no loop and
//! nothing it can reach.
//!
//! Its verdict is also deliberately narrower than the classifier's:
//!
//! * [`TriageVerdict::Answer`] narrows the delegation claim, exactly as a
//!   lexical `Answer` does — the model may reply and may hand off, and its pure
//!   board writes are refused in its own turn.
//! * Everything else leaves the gate exactly where the abstention left it.
//!
//! **It never mints a card.** A verdict does not become
//! [`MessageTriage::Track`], because the title a card is opened under is pinned
//! byte-for-byte between the REST handler and `chat_handler_card` (issue #463) —
//! a model-authored title would desynchronise them and orphan the card. That
//! also keeps the issue's own tie-breaker intact: a missed card costs one
//! follow-up message, a spurious card pollutes the board permanently.
//!
//! # Failure is silence, not an error
//!
//! Unreachable, slow, or unparseable all return [`TriageVerdict::Unavailable`],
//! and the caller keeps the answer Layer A already gave. The operator is waiting
//! on a reply; a classifier that cannot classify must cost nothing but its
//! timeout.
//!
//! [`triage_message`]: crate::company::task_intent::triage_message
//! [`triage_message_detailed`]: crate::company::task_intent::triage_message_detailed
//! [`MessageTriage::Chatter`]: crate::company::task_intent::MessageTriage::Chatter
//! [`MessageTriage::Track`]: crate::company::task_intent::MessageTriage::Track

use std::sync::Arc;
use std::time::Duration;

use tinyagents::harness::message::Message;
use tinyagents::harness::model::{ModelRequest, ModelResponse};

use crate::harness::HarnessDeps;
use crate::harness::build::model_for_tier;
use crate::harness::provider::HarnessModel;
use crate::ports::types::TokenUsage;

/// How long an escalation may wait on the model before it is abandoned.
///
/// Far tighter than the planning pass's two minutes, because the cost of
/// overrunning is different in kind: a planning card sits in a column, while an
/// operator sits watching a chat thread with no reply. Two seconds is a
/// classification budget, not a generation one — past it the deterministic
/// answer is simply better than a late one.
const TRIAGE_TIMEOUT: Duration = Duration::from_secs(2);

/// Output-token ceiling. The answer is one word; this is headroom for a model
/// that insists on punctuation, not room to explain itself.
const MAX_OUTPUT_TOKENS: u32 = 8;

/// Near-deterministic. Classification wants the same answer for the same
/// message, and #678 names `0.2` as `trigger_triage`'s own setting — kept rather
/// than rounded to zero so a genuinely borderline message is not forced.
const TEMPERATURE: f64 = 0.2;

/// What an escalation decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageVerdict {
    /// A question or read request. Narrows the delegation claim.
    Answer,
    /// Work, or something else the gate should not touch. Named separately from
    /// [`Self::Chatter`] so the log says which the model chose, but they act
    /// identically here — see the module docs on why work never mints a card.
    Work,
    /// Neither.
    Chatter,
    /// No answer: unreachable, too slow, or unreadable. The caller keeps the
    /// deterministic classification.
    Unavailable,
}

impl TriageVerdict {
    /// Whether this verdict narrows the delegation claim to answering-only.
    pub fn is_answer(&self) -> bool {
        matches!(self, Self::Answer)
    }

    /// Whether the model positively read this message as **conversation**
    /// (issue #984) — so the deterministic card paths open nothing for it.
    ///
    /// [`Work`](Self::Work) and [`Unavailable`](Self::Unavailable) are both
    /// false, and for the same reason from opposite directions: `Work` is the
    /// model saying a card is right, and `Unavailable` is the model saying
    /// nothing at all. Only an explicit `Chatter` may subtract a card, which is
    /// what keeps every degraded path byte-identical to the behaviour before
    /// this verdict had a consumer.
    pub fn is_chatter(&self) -> bool {
        matches!(self, Self::Chatter)
    }

    /// Reads a model's reply as a verdict, or [`Self::Unavailable`].
    ///
    /// A **closed vocabulary**, matched on the first recognised word rather than
    /// parsed. The model is asked for one word and usually gives one; anything
    /// else — a sentence, an apology, an empty string, a word not on the list —
    /// is not a classification and must not be guessed at. `Unavailable` is
    /// always the safe reading, because it means "keep what Layer A said".
    fn parse(text: &str) -> Self {
        for word in text
            .to_ascii_lowercase()
            .split(|c: char| !c.is_alphabetic())
        {
            match word {
                "answer" | "question" | "read" => return Self::Answer,
                "work" | "track" | "task" => return Self::Work,
                "chatter" | "neither" | "none" => return Self::Chatter,
                _ => continue,
            }
        }
        Self::Unavailable
    }
}

/// The system prompt. Framing from OpenHuman's `trigger_triage`, narrowed to
/// this one decision and to a one-word answer.
fn system_prompt() -> String {
    "You classify one message an operator sent to their company's chat.\n\
     \n\
     You do not act. You decide. Another component will carry out your decision.\n\
     \n\
     Answer with exactly one word, lowercase, and nothing else:\n\
     \n\
     - `answer` — a question, or a request to be told something about the \
     company's state. It should be replied to, not turned into work.\n\
     - `work` — a request to do something that should become a tracked task.\n\
     - `chatter` — neither: small talk, an acknowledgement, a remark.\n\
     \n\
     If you cannot tell, answer `chatter`. Being unsure is not a reason to \
     invent work."
        .to_string()
}

/// The system prompt, for the fixture that has to recognise a classification
/// request without consuming a scripted turn (issue #678).
#[cfg(test)]
pub fn system_prompt_for_test() -> String {
    system_prompt()
}

/// A model that classifies the messages the lexical layer abstained on.
///
/// Holds no runtime handle, mirroring
/// [`TaskPlanner`](crate::harness::planning::TaskPlanner): the caller already
/// has the handles metering needs, and an evaluator that owned the runtime back
/// would be a cycle that never frees.
pub struct TriageEvaluator {
    model: Arc<dyn HarnessModel>,
    model_name: String,
}

impl TriageEvaluator {
    /// Builds an evaluator over an explicit model.
    pub fn new(model: Arc<dyn HarnessModel>, model_name: impl Into<String>) -> Self {
        Self {
            model,
            model_name: model_name.into(),
        }
    }

    /// Builds the company's evaluator from the harness deps.
    ///
    /// # Why not a cheap tier
    ///
    /// #678 asks for "fast-model routing" — a second, cheaper tier so
    /// classification never pays orchestrator prices. That is the right
    /// instinct and the wrong mechanism, for a reason
    /// [`TaskPlanner::from_deps`](crate::harness::planning::TaskPlanner::from_deps)
    /// already documents: an abstract tier a tenant's `[inference].models` table
    /// does not map is **passed to their provider verbatim**
    /// (`request_plan` falls back to the tier string as the model id). Inventing
    /// a `fast-v1` and addressing it here would send an unknown model name to
    /// every BYOK tenant that had not opted in, and triage would fail on exactly
    /// the setups that otherwise work.
    ///
    /// So this takes the roster's own default — `model_override`, else the
    /// tier-less default. Cheapness comes from the *shape* of the call rather
    /// than from a tier: no tools, one short system prompt, one message, and
    /// [`MAX_OUTPUT_TOKENS`] of output. The orchestrator's turn runs
    /// `agentic-v1` with the full belt; this is already a fraction of it.
    ///
    /// A declarable cheap tier is worth having, but it needs the mapping to be
    /// *checked* rather than assumed — a follow-up, not a default.
    pub fn from_deps(deps: &HarnessDeps) -> Self {
        let model_name = deps
            .model_override
            .clone()
            .unwrap_or_else(|| model_for_tier(None));
        Self::new(deps.provider.clone(), model_name)
    }

    /// The provider slug this evaluator's usage is metered under, read live so a
    /// BYOK switch re-attributes the next escalation.
    pub fn provider_slug(&self) -> String {
        self.model.telemetry_provider_id()
    }

    /// The model this pass's usage is metered against, read live off the
    /// provider and already folded onto the closed vocabulary (issue #1749).
    /// `None` before the provider has issued a turn, or when it cannot name a
    /// model.
    pub fn model_slug(&self) -> Option<crate::metering::ModelSlug> {
        self.model.telemetry_model()
    }

    /// Classifies one operator message, with what the call cost.
    ///
    /// Never returns an error: every failure is [`TriageVerdict::Unavailable`]
    /// and the caller keeps the deterministic answer. The [`TokenUsage`] is
    /// still returned on a *parse* failure, because those tokens were really
    /// spent and must still be metered.
    pub async fn classify(&self, message: &str) -> (TriageVerdict, TokenUsage) {
        let request = ModelRequest {
            messages: vec![
                Message::system(system_prompt()),
                Message::user(message.to_string()),
            ],
            model: Some(self.model_name.clone()),
            temperature: Some(TEMPERATURE),
            max_tokens: Some(MAX_OUTPUT_TOKENS),
            ..ModelRequest::default()
        };
        let response = match tokio::time::timeout(TRIAGE_TIMEOUT, self.model.invoke(&(), request))
            .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                tracing::debug!(
                    error = %err,
                    "[triage] the model could not be reached; keeping the deterministic answer"
                );
                return (TriageVerdict::Unavailable, TokenUsage::default());
            }
            Err(_elapsed) => {
                tracing::debug!(
                    timeout_s = TRIAGE_TIMEOUT.as_secs(),
                    "[triage] the model did not answer in time; keeping the deterministic answer"
                );
                return (TriageVerdict::Unavailable, TokenUsage::default());
            }
        };
        let usage = usage_from(&response);
        (TriageVerdict::parse(&response.text()), usage)
    }
}

/// One classification of an abstained-on message, metering included.
///
/// A trait rather than the concrete [`TriageEvaluator`] so the delegation seam
/// depends on the *decision* and not on a model, a company id, a store and a
/// meter. That keeps the escalation testable from a scripted verdict, and keeps
/// the accounting handles out of a file that has no other business with them.
#[async_trait::async_trait]
pub trait TriageEscalation: Send + Sync {
    /// Classifies `message`, recording whatever the call cost.
    async fn classify(&self, message: &str) -> TriageVerdict;
}

/// The production escalation: a [`TriageEvaluator`] whose spend lands on the
/// company's usage and ledger before the verdict is returned.
pub struct MeteredTriage {
    evaluator: TriageEvaluator,
    company: crate::ports::types::CompanyId,
    store: Arc<dyn crate::ports::CompanyStore>,
    meter: Option<Arc<dyn crate::ports::usage::UsageMeter>>,
}

impl MeteredTriage {
    /// Wires an escalation for `company` from the harness deps.
    pub fn from_deps(deps: &HarnessDeps, company: crate::ports::types::CompanyId) -> Self {
        Self {
            evaluator: TriageEvaluator::from_deps(deps),
            company,
            store: deps.store.clone(),
            meter: deps.meter.clone(),
        }
    }
}

#[async_trait::async_trait]
impl TriageEscalation for MeteredTriage {
    async fn classify(&self, message: &str) -> TriageVerdict {
        let (verdict, usage) = self.evaluator.classify(message).await;
        // Metered even when the verdict is `Unavailable`: an unparseable reply
        // still burned tokens, and a classification we could not read is
        // precisely the spend an operator would want to see. The record call is
        // made whether or not a meter is wired — the ledger half must not be
        // lost to a host that only records spend it can prove.
        crate::metering::record_triage_usage(
            &usage,
            &self.evaluator.provider_slug(),
            self.evaluator.model_slug(),
            &self.company,
            self.store.as_ref(),
            self.meter.as_ref().map(|meter| meter.as_ref()),
        )
        .await;
        verdict
    }
}

/// Token spend for one escalation, including the cost the managed provider
/// reports on the wire. Mirrors the planning pass's reader.
fn usage_from(response: &ModelResponse) -> TokenUsage {
    let tokens = response.usage.unwrap_or_default();
    let cost_usd = response
        .raw
        .as_ref()
        .and_then(|raw| raw.pointer("/openhuman_usage_meta/charged_amount_usd"))
        .and_then(serde_json::Value::as_f64)
        .filter(|c| c.is_finite() && *c > 0.0)
        .unwrap_or(0.0);
    TokenUsage {
        input: tokens.input_tokens,
        output: tokens.output_tokens,
        cached_input: tokens.cache_read_tokens,
        cost_usd,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn the_three_verdicts_read_off_a_one_word_reply() {
        assert_eq!(TriageVerdict::parse("answer"), TriageVerdict::Answer);
        assert_eq!(TriageVerdict::parse("work"), TriageVerdict::Work);
        assert_eq!(TriageVerdict::parse("chatter"), TriageVerdict::Chatter);
    }

    /// Models add punctuation, capitals and the occasional preamble. None of
    /// that is a failure to classify.
    #[test]
    fn a_verdict_survives_the_shapes_a_model_actually_replies_in() {
        for (reply, want) in [
            ("Answer.", TriageVerdict::Answer),
            ("`work`", TriageVerdict::Work),
            ("  chatter\n", TriageVerdict::Chatter),
            ("The answer is: answer", TriageVerdict::Answer),
        ] {
            assert_eq!(TriageVerdict::parse(reply), want, "{reply:?}");
        }
    }

    /// The closed vocabulary. Anything unrecognised is not a classification, and
    /// guessing at it would spend the gate on noise.
    #[test]
    fn an_unreadable_reply_is_unavailable_rather_than_guessed() {
        for reply in [
            "",
            "   ",
            "I'm sorry, I can't help with that.",
            "42",
            "unknown",
        ] {
            assert_eq!(
                TriageVerdict::parse(reply),
                TriageVerdict::Unavailable,
                "{reply:?} is not a verdict"
            );
        }
    }

    /// Only `Answer` touches the gate. `Work` is named for the log and acts like
    /// `Chatter` here — a verdict never mints a card (see the module docs).
    #[test]
    fn only_answer_narrows_the_claim() {
        assert!(TriageVerdict::Answer.is_answer());
        assert!(!TriageVerdict::Work.is_answer());
        assert!(!TriageVerdict::Chatter.is_answer());
        assert!(!TriageVerdict::Unavailable.is_answer());
    }
}
