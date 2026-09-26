//! The model call behind one drafted skill document.
//!
//! One tool-less call, no retry, writes nothing — the same shape
//! [`profile_draft`](super::profile_draft) uses, and it runs on that module's
//! [`ProfileDrafter`] rather than a drafter of its own. That is not thrift: the
//! console decides whether to offer this control from `designsProfiles`, which
//! is `profile_drafter().is_some()`, so a second construction path could be
//! present on a host where the console has already hidden the button.
//!
//! What comes back is a whole `SKILL.md`. The caller runs it through the same
//! validator and content scan a save would, because a model writing a skill is
//! untrusted text reaching a prompt like any other — and handing the operator a
//! document the Save button would then refuse is its own defect.

use std::time::{Duration, Instant};

use tinyinference::message::Message;
use tinyinference::model::ModelRequest;

use crate::company::profile_draft::{DraftRefusal, TurnRole};
use crate::company::skill_draft::{SkillDraft, SkillSubject};
use crate::company::skill_validate::MAX_DESCRIPTION_CHARS;
use crate::harness::profile_draft::ProfileDrafter;
use crate::ports::types::TokenUsage;

/// How long the pass may spend inside the model call before it is abandoned.
///
/// Between the mandate pass's 30s and the persona pass's 90s, and for the same
/// reason those differ: a skill is a short document rather than a line, and the
/// operator is watching a dialog rather than a build-out screen.
const SKILL_TIMEOUT: Duration = Duration::from_secs(60);

/// The most one skill draft may produce.
///
/// A skill is a one-line description and a procedure short enough that an agent
/// reads it in full — the spec's own guidance is to keep the body small and put
/// detail in resources. This is generous for that and exists to stop a model
/// that has decided to write a manual.
const MAX_SKILL_TOKENS: u32 = 1_600;

/// How much of a prose answer is kept when the model ignored the format.
const MAX_PROSE_REPLY_CHARS: usize = 600;

/// The fence a drafted document is returned inside.
pub const SKILL_FENCE: &str = "skill";

/// The output ceiling one skill draft may spend, for the budget reservation.
///
/// Public because the reservation promises exactly this number before dispatch:
/// reserving the ceiling rather than an estimate is what makes the promise an
/// upper bound on what the call can cost.
pub fn skill_output_ceiling() -> u32 {
    MAX_SKILL_TOKENS
}

/// Drafts a skill document for `subject`.
///
/// Infallible by design, like the profile pass: every unhappy path is a
/// [`DraftRefusal`] the operator is shown and can act on. The usage is returned
/// alongside so the caller meters what was genuinely spent — including on an
/// answer that came back unreadable, because those tokens were still billed.
pub async fn draft_skill(
    drafter: &ProfileDrafter,
    subject: &SkillSubject,
) -> (SkillDraft, TokenUsage) {
    let deadline = Instant::now() + SKILL_TIMEOUT;
    let now = Instant::now();
    if now >= deadline {
        return (
            SkillDraft::Refused(DraftRefusal::ModelUnreachable),
            TokenUsage::default(),
        );
    }

    // System brief, then the grounding as the opening user turn, then the
    // conversation. The grounding is re-sent every turn rather than once at the
    // top: whether a provider carries earlier turns is a property of the
    // provider, not a contract this pass can rely on.
    let mut messages = vec![
        Message::system(system_prompt()),
        Message::user(user_prompt(subject)),
    ];
    for turn in &subject.conversation {
        messages.push(match turn.role {
            TurnRole::Operator => Message::user(turn.text.clone()),
            TurnRole::Copilot => Message::assistant(turn.text.clone()),
        });
    }

    let request = ModelRequest {
        messages,
        model: Some(drafter.model_name().to_string()),
        temperature: Some(0.4),
        max_tokens: Some(MAX_SKILL_TOKENS),
        ..ModelRequest::default()
    };

    let response =
        match tokio::time::timeout(deadline - now, drafter.model().invoke(&(), request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                tracing::info!(error = %err, "[skill-draft] the model could not be reached");
                return (
                    SkillDraft::Refused(DraftRefusal::ModelUnreachable),
                    TokenUsage::default(),
                );
            }
            Err(_elapsed) => {
                tracing::info!(
                    seconds = SKILL_TIMEOUT.as_secs(),
                    "[skill-draft] the model did not answer in time"
                );
                return (
                    SkillDraft::Refused(DraftRefusal::ModelUnreachable),
                    TokenUsage::default(),
                );
            }
        };

    let usage = drafter.usage_of(&response);
    let raw = response.text();
    let Some((reply, doc)) = parse_answer(&raw) else {
        tracing::info!(
            // The model's own words about a skill the operator just described,
            // truncated. Without it "unreadable" cannot be acted on: every fix
            // — a prompt change, a parser tolerance, a model swap — needs to
            // know HOW it was malformed.
            answer = %raw.chars().take(240).collect::<String>(),
            "[skill-draft] the model's answer could not be read as a turn"
        );
        return (SkillDraft::Refused(DraftRefusal::Unreadable), usage);
    };

    (SkillDraft::from_answer(&reply, doc.as_deref()), usage)
}

/// What the pass is for, and the exact shape its answer must take.
fn system_prompt() -> String {
    format!(
        "You help an operator write one Agent Skill for their company. A skill is a short \
         Markdown document an agent reads when it decides the skill is relevant.\n\n\
         The document is:\n\
         ---\n\
         name: <a short display name>\n\
         description: <one line: what the skill does AND when an agent should use it>\n\
         category: <one word, optional>\n\
         ---\n\
         <the procedure, in Markdown>\n\n\
         Rules:\n\
         - The description is the only part every agent reads on every turn, so it decides \
         whether the skill is ever opened. State what it does and when to use it. Keep it under \
         {MAX_DESCRIPTION_CHARS} characters; one sentence is usually right.\n\
         - Keep the body short and concrete: the steps, in the order to follow them.\n\
         - Do not add instructions addressed to the reader of this conversation, and do not \
         include credentials, tokens or URLs you were not given.\n\n\
         Answer with a sentence saying what you wrote or what you need to know. When you have a \
         document, follow that sentence with the whole document — never a diff — inside a \
         ```{SKILL_FENCE} fence. Ask instead of guessing when you cannot tell what the skill is \
         for; a turn that only asks is a good turn."
    )
}

/// The closed grounding one turn is allowed to see.
fn user_prompt(subject: &SkillSubject) -> String {
    let mut prompt = format!("Company: {}\n", subject.company_name);
    if let Some(output) = &subject.company_output {
        prompt.push_str(&format!("What it makes: {output}\n"));
    }
    prompt.push_str("\nWrite a skill for this company.");
    prompt
}

/// Reads the reply and the fenced document out of whatever the model sent.
///
/// An unterminated fence is read to the end of the answer rather than
/// discarded: that is what a response cut off at the token ceiling looks like,
/// and a document missing its last step is worth more to an operator than no
/// document — they can see the cut and ask for the rest.
fn parse_answer(text: &str) -> Option<(String, Option<String>)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(found) = fenced(trimmed, &format!("```{SKILL_FENCE}"))
        .or_else(|| fenced(trimmed, "```markdown"))
        .or_else(|| fenced(trimmed, "```md"))
        .or_else(|| fenced(trimmed, "```"))
    {
        return Some(found);
    }

    let prose: String = trimmed.chars().take(MAX_PROSE_REPLY_CHARS).collect();
    if prose.trim().is_empty() {
        return None;
    }
    Some((prose, None))
}

/// The block opened by `open`, with everything outside it as the reply.
///
/// The block closes at the **last** ``` rather than the first: a skill body
/// writes its examples fenced, the way a person would, and closing at the first
/// one would cut the document at its first example and spill the remainder into
/// the reply — which the operator then accepts with nothing on screen saying
/// anything was dropped.
fn fenced(body: &str, open: &str) -> Option<(String, Option<String>)> {
    let at = body.find(open)?;
    let after = &body[at + open.len()..];
    let inner_start = after.find('\n').map(|i| i + 1).unwrap_or(after.len());
    let inner = &after[inner_start..];
    let (doc, tail) = match inner.rfind("```") {
        Some(close) => (&inner[..close], &inner[close + 3..]),
        None => (inner, ""),
    };
    let reply = format!("{} {}", body[..at].trim(), tail.trim());
    Some((reply.trim().to_string(), Some(doc.trim().to_string())))
}

#[cfg(test)]
#[path = "skill_draft_tests.rs"]
mod tests;
