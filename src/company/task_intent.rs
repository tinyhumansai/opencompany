//! Detect an **actionable request** in an operator chat message, so the chat
//! handler can deterministically open a dashboard task card for it.
//!
//! OpenCompany's chat can already produce task cards — but only when the
//! orchestrator model chooses to call its `spawn_task` tool, and the
//! orchestrator brief biases toward just replying, so a "do this" ask often
//! leaves no visible work item. This module adds the deterministic half: when an
//! operator message is an actionable request ("build the landing page",
//! "can you set up the newsletter"), [`detect_task_intent`] returns a cleaned
//! task title and the handler opens a `backlog` card. The model's `spawn_task`
//! stays available for sub-tasks it wants to open on top.
//!
//! Conservative and cheap by design (no model call): it fires on imperative
//! requests and request-framed action asks, and stays silent on pure questions
//! ("what's our revenue?"), greetings/acknowledgements ("hi", "thanks"), and
//! neutral chatter — so the board fills with work, not small talk.

/// Bare greeting / acknowledgement messages that must never open a card, matched
/// against the whole message (punctuation stripped). Kept short and exact so a
/// longer message that merely *starts* with "ok" ("ok now build the page")
/// still qualifies.
const GREETINGS: &[&str] = &[
    "hi",
    "hii",
    "hey",
    "hello",
    "yo",
    "sup",
    "gm",
    "good morning",
    "good evening",
    "thanks",
    "thank you",
    "ty",
    "thx",
    "cheers",
    "ok",
    "okay",
    "k",
    "kk",
    "cool",
    "nice",
    "great",
    "awesome",
    "perfect",
    "got it",
    "gotcha",
    "sounds good",
    "sg",
    "np",
    "no problem",
    "yes",
    "yep",
    "yeah",
    "yup",
    "no",
    "nope",
    "sure",
    "done",
    "lgtm",
    "nvm",
];

/// Single-word imperative action verbs. A message whose first word is one of
/// these is an instruction to act.
const ACTION_VERBS: &[&str] = &[
    "build",
    "create",
    "make",
    "add",
    "fix",
    "find",
    "get",
    "write",
    "draft",
    "send",
    "schedule",
    "book",
    "research",
    "prepare",
    "plan",
    "generate",
    "design",
    "update",
    "review",
    "analyze",
    "analyse",
    "compile",
    "organize",
    "organise",
    "launch",
    "publish",
    "post",
    "email",
    "message",
    "call",
    "contact",
    "order",
    "buy",
    "hire",
    "recruit",
    "remove",
    "delete",
    "cancel",
    "start",
    "stop",
    "implement",
    "deploy",
    "configure",
    "install",
    "download",
    "upload",
    "summarize",
    "summarise",
    "list",
    "track",
    "monitor",
    "investigate",
    "handle",
    "fetch",
    "gather",
    "collect",
    "arrange",
    "coordinate",
    "outreach",
    "onboard",
    "migrate",
    "refactor",
    "test",
    "audit",
    "estimate",
    "calculate",
    "translate",
    "edit",
    "improve",
    "optimize",
    "optimise",
    "setup",
];

/// Multi-word imperative action phrases (checked as a prefix + trailing space).
const ACTION_PHRASES: &[&str] = &[
    "set up",
    "look into",
    "put together",
    "work on",
    "take care of",
    "follow up",
    "reach out",
    "figure out",
    "come up with",
    "draw up",
    "go through",
    "sign up",
    "spin up",
    "kick off",
    "roll out",
    "keep an eye on",
];

/// Request-framing prefixes. On their own these are polite wrappers ("can you",
/// "please"); combined with an action verb *anywhere* in the message they mark a
/// request to act (so "can you build X" fires but "can you tell me the revenue"
/// — no action verb — does not). Also stripped from the title.
const REQUEST_FRAMES: &[&str] = &[
    "can you ",
    "could you ",
    "would you ",
    "will you ",
    "please ",
    "pls ",
    "i need you to ",
    "i want you to ",
    "i'd like you to ",
    "i would like you to ",
    "we need you to ",
    "we want you to ",
    "i need to ",
    "i want to ",
    "we need to ",
    "we want to ",
    "i'd like to ",
    "we'd like to ",
    "let's ",
    "lets ",
    "go ahead and ",
    "make sure to ",
    "make sure you ",
    "can we ",
    "could we ",
];

/// Leading filler / acknowledgement / connective words that a real ask can open
/// with ("ok now build …", "and also fix …"). Stripped before the actionability
/// check and from the title, so the action verb underneath is seen. Kept
/// conservative to avoid eating a genuine first word.
const LEAD_INS: &[&str] = &[
    "ok ",
    "okay ",
    "k ",
    "now ",
    "also ",
    "then ",
    "so ",
    "alright ",
    "and ",
    "next ",
    "plus ",
    "hey ",
    "hi ",
    "yo ",
    "sure ",
    "actually ",
];

/// Max length of a generated task title.
const TITLE_MAX: usize = 80;

/// Returns a cleaned task title when `text` is an actionable request, else
/// `None`. See the module docs for the intent.
pub fn detect_task_intent(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_lowercase();
    // Whole-message greeting/ack check (strip trailing punctuation first).
    let bare = lower.trim_end_matches(['.', '!', '?', ' ']).trim();
    if GREETINGS.contains(&bare) {
        return None;
    }

    // Strip leading filler ("ok now …", "and also …") so the ask underneath is
    // judged on its own.
    let core = strip_lead_ins(&lower);
    if !is_actionable(core) {
        return None;
    }

    Some(to_title(trimmed))
}

/// Remove stacked leading filler words from a lowercased message.
fn strip_lead_ins(lower: &str) -> &str {
    let mut s = lower.trim_start();
    loop {
        match LEAD_INS
            .iter()
            .find(|f| s.starts_with(**f))
            .map(|f| f.len())
        {
            Some(len) => s = s[len..].trim_start(),
            None => return s,
        }
    }
}

/// Whether the lowercased message is an actionable request.
fn is_actionable(lower: &str) -> bool {
    // A leading imperative verb/phrase is unambiguously an instruction.
    if starts_with_action(lower) {
        return true;
    }
    // A request frame counts only when it wraps an actual action verb, so a
    // framed question ("can you tell me …") does not qualify.
    if REQUEST_FRAMES.iter().any(|f| lower.starts_with(f)) && contains_action(lower) {
        return true;
    }
    false
}

/// The message's first word (or a leading multi-word phrase) is an action verb.
fn starts_with_action(lower: &str) -> bool {
    if ACTION_PHRASES.iter().any(|p| starts_with_word(lower, p)) {
        return true;
    }
    let first = lower.split_whitespace().next().unwrap_or("");
    // Trim trailing punctuation on the first word ("build," / "fix:").
    let first = first.trim_end_matches([',', ':', ';', '.', '!']);
    ACTION_VERBS.contains(&first)
}

/// An action verb/phrase appears anywhere (used behind a request frame).
fn contains_action(lower: &str) -> bool {
    if ACTION_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| ACTION_VERBS.contains(&word))
}

/// True when `lower` begins with `phrase` followed by a word boundary.
fn starts_with_word(lower: &str, phrase: &str) -> bool {
    lower == phrase
        || lower
            .strip_prefix(phrase)
            .is_some_and(|rest| rest.starts_with(' '))
}

/// Build a task title: strip a leading request frame, drop trailing
/// punctuation, upper-case the first letter, and bound the length.
fn to_title(original: &str) -> String {
    let mut s = original.trim();
    // Strip stacked leading filler ("ok now …") and request-framing prefixes
    // ("please can you …") for a clean imperative title.
    loop {
        let lower = s.to_lowercase();
        let hit = LEAD_INS
            .iter()
            .chain(REQUEST_FRAMES.iter())
            .find(|f| lower.starts_with(**f))
            .map(|f| f.len());
        match hit {
            Some(len) => s = s[len..].trim_start(),
            None => break,
        }
    }
    let s = s.trim_end_matches(['?', '.', '!', ' ']).trim();
    if s.is_empty() {
        // A frame with no residual body ("please.") — fall back to the original.
        return truncate(original.trim(), TITLE_MAX);
    }
    let capped = truncate(s, TITLE_MAX);
    capitalize_first(&capped)
}

/// Upper-case the first character, leaving the rest untouched.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// UTF-8-safe truncation to at most `max` chars, appending `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_imperative_is_actionable() {
        for msg in [
            "Build the landing page",
            "create a new email campaign",
            "fix the checkout bug",
            "find three suppliers for widgets",
            "set up the weekly newsletter",
            "look into why signups dropped",
            "research competitors in the EU",
        ] {
            assert!(detect_task_intent(msg).is_some(), "should fire: {msg}");
        }
    }

    #[test]
    fn framed_request_with_action_verb_is_actionable() {
        assert_eq!(
            detect_task_intent("Can you build the landing page?").as_deref(),
            Some("Build the landing page")
        );
        assert!(detect_task_intent("please set up the newsletter").is_some());
        assert!(detect_task_intent("I need you to draft the pitch").is_some());
        assert!(detect_task_intent("let's launch the beta").is_some());
    }

    #[test]
    fn pure_questions_do_not_fire() {
        for msg in [
            "what's our revenue this month?",
            "how many users signed up?",
            "who is on the growth desk?",
            "can you tell me the latest numbers?", // framed, but no action verb
            "is the campaign live?",
            "why did signups drop?",
        ] {
            assert!(detect_task_intent(msg).is_none(), "should not fire: {msg}");
        }
    }

    #[test]
    fn greetings_and_acks_do_not_fire() {
        for msg in [
            "hi",
            "Hello!",
            "hey",
            "thanks",
            "thank you",
            "ok",
            "okay",
            "cool",
            "got it",
            "sounds good",
            "perfect",
            "yes",
            "no",
            "sure",
            "done",
        ] {
            assert!(detect_task_intent(msg).is_none(), "should not fire: {msg}");
        }
    }

    #[test]
    fn title_strips_frame_caps_and_trims() {
        assert_eq!(
            detect_task_intent("please can you fix the login bug!").as_deref(),
            Some("Fix the login bug")
        );
        assert_eq!(
            detect_task_intent("go ahead and publish the blog post").as_deref(),
            Some("Publish the blog post")
        );
    }

    #[test]
    fn title_is_bounded() {
        let long = format!("build {}", "a very detailed feature ".repeat(20));
        let title = detect_task_intent(&long).expect("actionable");
        assert!(title.chars().count() <= TITLE_MAX + 1, "bounded: {title}");
    }

    #[test]
    fn empty_and_whitespace_do_not_fire() {
        assert!(detect_task_intent("").is_none());
        assert!(detect_task_intent("   ").is_none());
    }

    #[test]
    fn ack_prefix_then_request_still_fires() {
        // "ok" as a whole message is an ack, but not as a prefix of a real ask.
        assert!(detect_task_intent("ok now build the dashboard").is_some());
    }
}
