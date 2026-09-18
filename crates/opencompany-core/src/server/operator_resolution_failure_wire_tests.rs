use super::{ChatHistoryMessageDto, turn_failure_notice};
use crate::company::inference::copy;
use crate::server::chat_history::MessageView;

/// A classified `MessageView` (as `MessageView::project` would build one
/// from a stored `AgentReply` whose `text` is the bare X9 sentence) turns
/// into exactly the five documented keys, camelCased, with nothing else
/// added or renamed.
#[test]
fn a_classified_failure_serialises_the_documented_wire_keys() {
    let sentence = copy::with_agent_marker(
        copy::pair_broken("Researcher", "acme", copy::ProviderGone::Removed),
        "researcher",
    );
    let resolution = copy::classify(&sentence).expect("this sentence classifies");

    let mut view = MessageView::for_test("1", "system", &resolution.message, Vec::new());
    view.resolution_user_facing = true;
    view.resolution_code = Some(resolution.code.to_string());
    view.resolution_pair_agent_id = resolution.pair_agent_id.clone();
    view.resolution_provider_slug = resolution.provider_slug.clone();

    let dto = ChatHistoryMessageDto::from(view);
    let json = serde_json::to_value(&dto).unwrap();

    assert_eq!(json["userFacing"], true);
    assert_eq!(json["code"], "pair_provider_removed");
    assert_eq!(json["message"], resolution.message);
    assert_eq!(json["pairAgentId"], "researcher");
    assert_eq!(json["providerSlug"], "acme");
    // The full sentence, verbatim — never re-derived or paraphrased, and
    // with no trace of the hidden agent-id marker.
    assert!(!json["message"].as_str().unwrap().contains('\u{0}'));
}

/// An ordinary reply — the overwhelmingly common case — carries none of
/// the five keys at all, not even as `null`, so an old console reading
/// this DTO sees exactly the shape it always has.
#[test]
fn an_ordinary_reply_carries_none_of_the_five_keys() {
    let view = MessageView::for_test("1", "researcher", "Here's the summary.", Vec::new());
    let dto = ChatHistoryMessageDto::from(view);
    let json = serde_json::to_value(&dto).unwrap();

    for key in [
        "userFacing",
        "code",
        "message",
        "pairAgentId",
        "providerSlug",
    ] {
        assert!(json.get(key).is_none(), "{key} must be absent: {json}");
    }
}

/// A generic (non-resolution) failure — a rate limit, an empty response,
/// a tool timeout — is not reclassified as a resolution failure just
/// because it also failed a turn: `userFacing` stays absent.
#[test]
fn a_generic_failure_is_not_marked_user_facing() {
    let text = turn_failure_notice("the tool call exceeded its wall-clock budget");
    assert!(
        copy::classify(&text).is_none(),
        "a generic failure's wrapped text must not classify"
    );

    let view = MessageView::for_test("1", "system", &text, Vec::new());
    let dto = ChatHistoryMessageDto::from(view);
    let json = serde_json::to_value(&dto).unwrap();
    assert!(json.get("userFacing").is_none(), "{json}");
}

/// One test per code (KR-L2-03's explicit ask): every producible code
/// round-trips through the same DTO with the exact wire spelling the
/// console's `TURN_FAILURE_CODES` list names.
#[test]
fn every_producible_code_reaches_the_wire_unmodified() {
    let cases = [
        (copy::nothing_resolved_for_company(), "no_model_chosen"),
        (
            copy::pair_broken("Researcher", "Acme", copy::ProviderGone::TurnedOff),
            "pair_provider_off",
        ),
        (
            copy::default_broken("Acme", copy::ProviderGone::Removed),
            "default_provider_removed",
        ),
        (
            copy::default_broken("Acme", copy::ProviderGone::TurnedOff),
            "default_provider_off",
        ),
        (
            copy::provider_has_no_key("Researcher", "Acme"),
            "provider_no_key",
        ),
    ];
    for (sentence, code) in cases {
        let resolution = copy::classify(&sentence).unwrap_or_else(|| panic!("{sentence}"));
        let mut view = MessageView::for_test("1", "system", &resolution.message, Vec::new());
        view.resolution_user_facing = true;
        view.resolution_code = Some(resolution.code.to_string());
        let dto = ChatHistoryMessageDto::from(view);
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["code"], code, "{sentence}");
        assert_eq!(json["userFacing"], true, "{sentence}");
    }
}

/// A bound-harness failure reaches the console as a classified reply rather
/// than the generic notice, whose "Send the message again to retry" closing
/// is wrong advice for a turn that never reached a model.
#[test]
fn a_harness_binding_failure_replaces_the_generic_notice() {
    let detail = "configuration error: agent `researcher` is bound to harness `claude-code`, \
                  but it is an ACP harness and this build has no ACP transport wired.";

    let resolution = copy::classify(detail).expect("a harness failure classifies");
    assert_eq!(resolution.code, "harness_unavailable");
    assert!(
        !turn_failure_notice(detail).contains("claude-code"),
        "the generic notice is exactly what loses the harness name"
    );

    let mut view = MessageView::for_test("1", "system", &resolution.message, Vec::new());
    view.resolution_user_facing = true;
    view.resolution_code = Some(resolution.code.to_string());
    view.resolution_pair_agent_id = resolution.pair_agent_id.clone();

    let dto = ChatHistoryMessageDto::from(view);
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["userFacing"], true);
    assert_eq!(json["code"], "harness_unavailable");
    assert_eq!(json["pairAgentId"], "researcher");
    assert!(
        json["message"].as_str().unwrap().contains("claude-code"),
        "the harness must be named on the wire: {json}"
    );
    assert!(
        !json["message"]
            .as_str()
            .unwrap()
            .contains("Send the message again"),
        "the retry advice must not survive: {json}"
    );
}
