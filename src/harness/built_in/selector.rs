//! The per-message responder selection for `auto` channels (issue #1835).
//!
//! # The rung this adds
//!
//! An [`Auto`](crate::ports::types::ResponderMode::Auto) channel has no lead:
//! nobody outranks anybody, so "who answers this?" is decided **per message**
//! rather than at creation time. This module is that decision — a single
//! tool-less model call handed the message and the channel's own membership
//! (id, role, description — the same fields `GET …/team` serves), answering
//! with exactly one member id.
//!
//! It sits **below** an `@`-mention and **above** the deterministic fallback:
//! a named teammate still outranks everything
//! ([`mention_responder`](crate::runtime::mentions::mention_responder)), and
//! wherever this call cannot run — the default build, the small-talk fast
//! path, a failure, a timeout — the answer is
//! [`desk_default_responder`](crate::runtime::delegation_tools::desk_default_responder),
//! the channel's first roster member, which is exactly what a lead desk would
//! have answered. The worst case of the new rung is the old rung.
//!
//! # It decides, it does not act
//!
//! Same framing as the triage escalation this module is modelled on
//! ([`triage`](super::triage), issue #678): the request carries **no tools at
//! all**, so there is no loop and nothing it can reach. The message is shown
//! to it as routing *data* — a directive inside the message can influence
//! which member answers (that is routing working, not failing: "can someone
//! from design look at this" *should* land on design), but it cannot make the
//! selector do anything except name a member.
//!
//! # Failure is silence, not an error
//!
//! Unreachable, slow, unparseable, or an id outside the membership all return
//! [`SelectorVerdict::Unavailable`], and the caller keeps the deterministic
//! answer. The operator is waiting on a reply; a selector that cannot select
//! must cost nothing but its timeout.

use std::sync::Arc;
use std::time::Duration;

use tinyagents::harness::message::Message;
use tinyagents::harness::model::{ModelRequest, ModelResponse};

use crate::harness::HarnessDeps;
use crate::harness::build::model_for_tier;
use crate::harness::provider::HarnessModel;
use crate::ports::types::TokenUsage;

/// How long a selection may wait on the model before it is abandoned.
///
/// The same class of budget as the triage escalation's two seconds — an
/// operator is watching a chat thread with no reply — with one second of
/// headroom for the larger input (a roster block rather than one message).
/// Past it the deterministic fallback is simply better than a late pick.
const SELECTOR_TIMEOUT: Duration = Duration::from_secs(3);

/// Output-token ceiling. The answer is one member id; this is headroom for a
/// long snake_case id and stray punctuation, not room to explain the choice.
const MAX_OUTPUT_TOKENS: u32 = 24;

/// Deterministic. Routing wants the same pick for the same message — an
/// operator who sends the same question twice should not watch it land on two
/// different teammates. Unlike triage there is no upstream setting to honour,
/// so this is zero rather than near-zero.
const TEMPERATURE: f64 = 0.0;

/// One channel member as the selector sees it: the id it must answer with and
/// the role/description it judges fit by — the same fields the console's
/// members pane renders, so the selector reasons from what an operator reads.
#[derive(Debug, Clone)]
pub struct SelectorCandidate {
    /// The roster id — the only valid answer token.
    pub id: String,
    /// The teammate's role, e.g. "Backend Engineer".
    pub role: String,
    /// The teammate's mandate, when one is declared.
    pub description: Option<String>,
}

/// What a selection decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorVerdict {
    /// One of the candidates, by id — guaranteed by [`SelectorVerdict::parse`]
    /// to be a member of the candidate set it was parsed against.
    Member(String),
    /// No usable pick: unreachable, slow, unparseable, or an id outside the
    /// membership. The caller keeps the deterministic fallback.
    Unavailable,
}

impl SelectorVerdict {
    /// Reads a model reply into a verdict, **clamped to `candidates`**.
    ///
    /// Tolerates the decoration small models add around a token — whitespace,
    /// wrapping quotes or backticks, a trailing period — and matches the id
    /// case-insensitively, answering with the candidate's own casing. Anything
    /// that is not one of the candidates is [`Self::Unavailable`], never a
    /// guess: an out-of-set id routed verbatim would address a turn to a
    /// teammate the channel does not contain.
    pub fn parse(text: &str, candidates: &[SelectorCandidate]) -> Self {
        let token = text
            .trim()
            .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
            .trim_end_matches('.')
            .trim();
        candidates
            .iter()
            .find(|c| c.id.eq_ignore_ascii_case(token))
            .map(|c| SelectorVerdict::Member(c.id.clone()))
            .unwrap_or(SelectorVerdict::Unavailable)
    }
}

/// The selection instruction. Kept in the triage escalation's voice: it
/// decides, it does not act, and unsureness has a named safe answer.
fn system_prompt() -> String {
    "You route one message in a group channel to the member best fit to answer it. \
     You do not act. You do not reply to the message. You only decide who should.\n\
     \n\
     You are given the channel's members — id, role, and what they own — and the \
     message. Judge fit by the message's subject against each member's role and \
     mandate.\n\
     \n\
     Answer with exactly one member id from the list, and nothing else. \
     If no member is clearly the better fit, answer the first listed id."
        .to_string()
}

/// The system prompt, for the fixture that has to recognise a selection
/// request without consuming a scripted turn (the same seam
/// [`triage::system_prompt_for_test`](super::triage::system_prompt_for_test)
/// exists for).
#[cfg(test)]
pub fn system_prompt_for_test() -> String {
    system_prompt()
}

/// The user message: the membership block, then the message being routed —
/// data laid out for a judgement, in the order the judgement reads it.
fn selection_request(message: &str, candidates: &[SelectorCandidate]) -> String {
    let mut out = String::from("Channel members:\n");
    for c in candidates {
        out.push_str("- ");
        out.push_str(&c.id);
        out.push_str(" — ");
        out.push_str(&c.role);
        if let Some(description) = c.description.as_deref().filter(|d| !d.trim().is_empty()) {
            out.push_str(": ");
            out.push_str(description);
        }
        out.push('\n');
    }
    out.push_str("\nMessage:\n");
    out.push_str(message);
    out
}

/// A model that picks the best-fit member for one message.
///
/// Holds no runtime handle, mirroring [`TriageEvaluator`](super::triage) and
/// the planning pass: the caller already has the handles metering needs, and
/// an evaluator that owned the runtime back would be a cycle that never frees.
pub struct ResponderSelector {
    model: Arc<dyn HarnessModel>,
    model_name: String,
}

impl ResponderSelector {
    /// Builds a selector over an explicit model.
    pub fn new(model: Arc<dyn HarnessModel>, model_name: impl Into<String>) -> Self {
        Self {
            model,
            model_name: model_name.into(),
        }
    }

    /// Builds the company's selector from the harness deps.
    ///
    /// Takes the roster's own default model — `model_override`, else the
    /// tier-less default — rather than inventing a cheap tier, for the reason
    /// [`TriageEvaluator::from_deps`](super::triage::TriageEvaluator::from_deps)
    /// documents at length: an abstract tier a tenant's `[inference].models`
    /// table does not map is passed to their provider verbatim, so a made-up
    /// `fast-v1` would fail on exactly the BYOK setups that otherwise work.
    /// Cheapness comes from the shape of the call — no tools, one short system
    /// prompt, one message, [`MAX_OUTPUT_TOKENS`] of output.
    pub fn from_deps(deps: &HarnessDeps) -> Self {
        let model_name = deps
            .model_override
            .clone()
            .unwrap_or_else(|| model_for_tier(None));
        Self::new(deps.provider.clone(), model_name)
    }

    /// The provider slug this selector's usage is metered under, read live so
    /// a BYOK switch re-attributes the next selection.
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

    /// Picks the best-fit member for one message, with what the call cost.
    ///
    /// Never returns an error: every failure is
    /// [`SelectorVerdict::Unavailable`] and the caller keeps the deterministic
    /// fallback. The [`TokenUsage`] is still returned on a *parse* failure,
    /// because those tokens were really spent and must still be metered.
    pub async fn select(
        &self,
        message: &str,
        candidates: &[SelectorCandidate],
    ) -> (SelectorVerdict, TokenUsage) {
        let request = ModelRequest {
            messages: vec![
                Message::system(system_prompt()),
                Message::user(selection_request(message, candidates)),
            ],
            model: Some(self.model_name.clone()),
            temperature: Some(TEMPERATURE),
            max_tokens: Some(MAX_OUTPUT_TOKENS),
            ..ModelRequest::default()
        };
        let response =
            match tokio::time::timeout(SELECTOR_TIMEOUT, self.model.invoke(&(), request)).await {
                Ok(Ok(response)) => response,
                Ok(Err(err)) => {
                    tracing::debug!(
                        error = %err,
                        "[selector] the model could not be reached; keeping the deterministic \
                         fallback"
                    );
                    return (SelectorVerdict::Unavailable, TokenUsage::default());
                }
                Err(_elapsed) => {
                    tracing::debug!(
                        timeout_s = SELECTOR_TIMEOUT.as_secs(),
                        "[selector] the model did not answer in time; keeping the deterministic \
                         fallback"
                    );
                    return (SelectorVerdict::Unavailable, TokenUsage::default());
                }
            };
        let usage = usage_from(&response);
        (SelectorVerdict::parse(&response.text(), candidates), usage)
    }
}

/// The production selection: a [`ResponderSelector`] whose spend lands on the
/// company's usage and ledger before the verdict is returned.
pub struct MeteredSelector {
    selector: ResponderSelector,
    company: crate::ports::types::CompanyId,
    store: Arc<dyn crate::ports::CompanyStore>,
    meter: Option<Arc<dyn crate::ports::usage::UsageMeter>>,
}

impl MeteredSelector {
    /// Wires a selection for `company` from the harness deps.
    pub fn from_deps(deps: &HarnessDeps, company: crate::ports::types::CompanyId) -> Self {
        Self {
            selector: ResponderSelector::from_deps(deps),
            company,
            store: deps.store.clone(),
            meter: deps.meter.clone(),
        }
    }

    /// Picks the best-fit member, recording whatever the call cost.
    ///
    /// Metered even when the verdict is `Unavailable`: an unparseable reply
    /// still burned tokens, and a pick we could not read is precisely the
    /// spend an operator would want to see. The record call is made whether or
    /// not a meter is wired — the ledger half must not be lost to a host that
    /// only records spend it can prove.
    pub async fn select(&self, message: &str, candidates: &[SelectorCandidate]) -> SelectorVerdict {
        let (verdict, usage) = self.selector.select(message, candidates).await;
        crate::metering::record_selector_usage(
            &usage,
            &self.selector.provider_slug(),
            self.selector.model_slug(),
            &self.company,
            self.store.as_ref(),
            self.meter.as_ref().map(|meter| meter.as_ref()),
        )
        .await;
        verdict
    }
}

/// Token spend for one selection, including the cost the managed provider
/// reports on the wire. Mirrors the triage escalation's reader.
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
mod tests {
    use super::*;

    fn candidates() -> Vec<SelectorCandidate> {
        vec![
            SelectorCandidate {
                id: "backend_engineer".to_string(),
                role: "Backend Engineer".to_string(),
                description: Some("Owns the API surface.".to_string()),
            },
            SelectorCandidate {
                id: "designer".to_string(),
                role: "Product Designer".to_string(),
                description: None,
            },
        ]
    }

    /// The decoration small models add around a token — quotes, backticks, a
    /// trailing period, case drift — parses to the candidate's own id.
    #[test]
    fn parse_tolerates_decoration_and_answers_the_candidates_casing() {
        for reply in [
            "backend_engineer",
            " backend_engineer \n",
            "\"backend_engineer\"",
            "`backend_engineer`",
            "Backend_Engineer.",
        ] {
            assert_eq!(
                SelectorVerdict::parse(reply, &candidates()),
                SelectorVerdict::Member("backend_engineer".to_string()),
                "reply {reply:?} should parse"
            );
        }
    }

    /// An id outside the membership is `Unavailable`, never routed verbatim —
    /// an out-of-set pick would address a turn to a teammate the channel does
    /// not contain. Revert the clamp in [`SelectorVerdict::parse`] and this
    /// answers `Member("ceo")`.
    #[test]
    fn parse_clamps_to_the_candidate_set() {
        assert_eq!(
            SelectorVerdict::parse("ceo", &candidates()),
            SelectorVerdict::Unavailable
        );
        assert_eq!(
            SelectorVerdict::parse("", &candidates()),
            SelectorVerdict::Unavailable
        );
        assert_eq!(
            SelectorVerdict::parse(
                "backend_engineer because the message is about the API",
                &candidates()
            ),
            SelectorVerdict::Unavailable,
            "an answer with an explanation attached is not an id"
        );
    }

    /// The request lays the membership out id-first, so the only valid answer
    /// tokens are on the page, and frames the message as the thing being
    /// routed rather than instructions to follow.
    #[test]
    fn selection_request_names_ids_roles_and_the_message() {
        let request = selection_request("who owns the login flow?", &candidates());
        assert!(request.contains("- backend_engineer — Backend Engineer: Owns the API surface."));
        assert!(request.contains("- designer — Product Designer\n"));
        assert!(request.contains("Message:\nwho owns the login flow?"));
    }
}
