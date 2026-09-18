use super::*;

#[test]
fn no_model_for_provider_names_the_provider() {
    assert_eq!(
        no_model_for_provider("TinyHumans"),
        "Choose a model for TinyHumans before saving."
    );
}

#[test]
fn nothing_resolved_names_the_agent_and_the_settings_path() {
    assert_eq!(
        nothing_resolved("Researcher"),
        "No model is chosen. Choose a provider and model for Researcher, \
         or set the company default in Connections → API Keys → LLM."
    );
}

#[test]
fn nothing_resolved_for_company_names_no_agent() {
    let text = nothing_resolved_for_company();
    assert!(text.starts_with("No model is chosen for this company."));
    assert!(text.contains("Connections → API Keys → LLM"));
}

#[test]
fn provider_has_no_key_names_the_agent_provider_and_settings_path() {
    assert_eq!(
        provider_has_no_key("Researcher", "Anthropic"),
        "Researcher uses Anthropic, which has no key. Add one in \
         Connections → API Keys → LLM, or choose another provider and \
         model for Researcher."
    );
}

#[test]
fn pair_broken_distinguishes_removed_from_turned_off() {
    assert_eq!(
        pair_broken("Researcher", "Anthropic", ProviderGone::Removed),
        "Researcher uses Anthropic, which is removed. Choose another \
         provider and model for Researcher, or clear its model to use \
         the company default."
    );
    assert_eq!(
        pair_broken("Researcher", "Anthropic", ProviderGone::TurnedOff),
        "Researcher uses Anthropic, which is turned off. Choose another \
         provider and model for Researcher, or clear its model to use \
         the company default."
    );
}

#[test]
fn default_broken_names_the_provider_and_the_settings_path() {
    assert_eq!(
        default_broken("Anthropic", ProviderGone::Removed),
        "The company default uses Anthropic, which is removed. Choose a \
         new default in Connections → API Keys → LLM."
    );
}

// ---- classify (keys rework #2306, round-2 review KR-L2-03) -------------

#[test]
fn classify_recognises_every_producible_code() {
    let cases: [(String, &str); 6] = [
        (nothing_resolved("Researcher"), NO_MODEL_CHOSEN_CODE),
        (nothing_resolved_for_company(), NO_MODEL_CHOSEN_CODE),
        (
            pair_broken("Researcher", "acme", ProviderGone::Removed),
            PAIR_PROVIDER_REMOVED_CODE,
        ),
        (
            pair_broken("Researcher", "Acme", ProviderGone::TurnedOff),
            PAIR_PROVIDER_OFF_CODE,
        ),
        (
            default_broken("Acme", ProviderGone::Removed),
            DEFAULT_PROVIDER_REMOVED_CODE,
        ),
        (
            default_broken("Acme", ProviderGone::TurnedOff),
            DEFAULT_PROVIDER_OFF_CODE,
        ),
    ];
    for (sentence, code) in cases {
        let got = classify(&sentence).unwrap_or_else(|| panic!("did not classify: {sentence}"));
        assert_eq!(got.code, code, "{sentence}");
        assert_eq!(got.message, sentence);
    }
    let no_key = provider_has_no_key("Researcher", "Acme");
    let got = classify(&no_key).expect("provider_has_no_key classifies");
    assert_eq!(got.code, PROVIDER_NO_KEY_CODE);
}

#[test]
fn classify_recovers_the_slug_only_for_a_removed_pair() {
    let removed = pair_broken("Researcher", "acme", ProviderGone::Removed);
    assert_eq!(
        classify(&removed).unwrap().provider_slug.as_deref(),
        Some("acme"),
        "the Removed sentence names the raw slug, with nothing else to name"
    );

    let off = pair_broken("Researcher", "Acme", ProviderGone::TurnedOff);
    assert_eq!(
        classify(&off).unwrap().provider_slug,
        None,
        "TurnedOff names a display label, not a slug — nothing to recover"
    );

    let default_removed = default_broken("Acme", ProviderGone::Removed);
    assert_eq!(
        classify(&default_removed).unwrap().provider_slug,
        None,
        "a bare-slug default already names a slug in the sentence, but \
         this classifier only recovers one for the pair case today"
    );
}

#[test]
fn with_agent_marker_round_trips_through_classify_and_never_shows() {
    let sentence = pair_broken("Researcher", "acme", ProviderGone::Removed);
    let marked = with_agent_marker(sentence.clone(), "researcher");
    assert_ne!(marked, sentence, "the marker must actually be attached");

    let got = classify(&marked).expect("still classifies with the marker attached");
    assert_eq!(got.code, PAIR_PROVIDER_REMOVED_CODE);
    assert_eq!(
        got.message, sentence,
        "the marker must never appear in the displayed message"
    );
    assert_eq!(got.pair_agent_id.as_deref(), Some("researcher"));
    assert_eq!(got.provider_slug.as_deref(), Some("acme"));
}

#[test]
fn classify_without_a_marker_leaves_pair_agent_id_unset() {
    let sentence = pair_broken("Researcher", "Acme", ProviderGone::TurnedOff);
    let got = classify(&sentence).unwrap();
    assert_eq!(got.pair_agent_id, None);
}

#[test]
fn classify_ignores_every_other_failure() {
    for detail in [
        "This turn couldn't be finished — the AI provider is rate-limiting requests.",
        "the tool timed out",
        "inference returned 500 Internal Server Error",
        "",
    ] {
        assert!(classify(detail).is_none(), "{detail}");
    }
}

// ---- the harness arm --------------------------------------------------

/// The shape `HarnessRouter::engine_for` writes when a declared harness has
/// no engine here. Spelled out rather than imported because `classify` is
/// ungated and `harness` is not; `harness/router_tests.rs` drives the real
/// router against the real classifier so the two spellings cannot drift.
const ROUTER_NO_ENGINE: &str = "agent `researcher` is bound to harness `claude-code`, but it is \
     an ACP harness and this build has no ACP transport wired.";

#[test]
fn a_harness_binding_failure_classifies_and_names_the_agent() {
    let got = classify(ROUTER_NO_ENGINE).expect("the router sentence classifies");
    assert_eq!(got.code, HARNESS_UNAVAILABLE_CODE);
    assert_eq!(got.pair_agent_id.as_deref(), Some("researcher"));
    assert_eq!(got.provider_slug, None);
    assert!(
        got.message.starts_with("agent `researcher`"),
        "{}",
        got.message
    );
    assert!(
        got.message.contains("no ACP transport wired"),
        "{}",
        got.message
    );
    assert!(got.message.ends_with(HARNESS_RETRY_NOTE), "{}", got.message);
}

#[test]
fn the_display_prefix_never_reaches_the_reader() {
    let wrapped = format!("configuration error: {ROUTER_NO_ENGINE}");
    let got = classify(&wrapped).expect("a wrapped sentence still classifies");
    assert!(
        !got.message.contains("configuration error"),
        "the error-type prefix is a diagnostic: {}",
        got.message
    );
    assert_eq!(got.message, classify(ROUTER_NO_ENGINE).unwrap().message);
}

#[test]
fn reclassifying_the_stored_harness_sentence_changes_nothing() {
    let once = classify(ROUTER_NO_ENGINE).unwrap();
    let twice = classify(&once.message).unwrap();
    assert_eq!(once, twice, "every read re-classifies the stored text");
}

#[test]
fn a_harness_sentence_naming_no_agent_is_not_classified() {
    assert!(classify("agent `` is bound to harness `deep`, but nothing.").is_none());
    assert!(classify("bound to harness `deep`").is_none());
}

#[test]
fn a_reason_tail_containing_uses_stays_a_harness_failure() {
    let sentence = "agent `researcher` is bound to harness `runner`, but it uses \
                    `transport = \"runner\"` and this build has no runner transport wired yet.";
    let got = classify(sentence).expect("classifies");
    assert_eq!(
        got.code, HARNESS_UNAVAILABLE_CODE,
        "a ` uses ` in the reason must not steal this into a pair arm"
    );
}

/// The router's other sentence shape — a warm-up failure rather than no
/// engine at all — classifies the same way.
#[test]
fn a_warm_up_failure_sentence_classifies_too() {
    let sentence = "agent `researcher` is bound to harness `claude-code`, whose last \
                    warm-up failed: could not start `claude`: No such file or directory.";
    let got = classify(sentence).expect("the warm-up-failure shape classifies");
    assert_eq!(got.code, HARNESS_UNAVAILABLE_CODE);
    assert_eq!(got.pair_agent_id.as_deref(), Some("researcher"));
    assert_eq!(
        got.message,
        "agent `researcher` is bound to harness `claude-code`, whose last warm-up failed. \
         Retrying will not help until that harness can run."
    );
}

/// The reason a lane's warm-up gives is `{err}` from that lane — a path, a
/// command line, whatever the engine said — and it is cut before the sentence
/// reaches a person.
#[test]
fn the_warm_up_reason_never_reaches_the_message() {
    let sentence = "agent `researcher` is bound to harness `claude-code`, whose last \
                    warm-up failed: could not start `/Users/someone/.secrets/claude`: \
                    No such file or directory.";
    let got = classify(sentence).expect("classifies");
    assert!(
        !got.message.contains("/Users/someone"),
        "the reason must not survive into chat: {}",
        got.message
    );
    assert!(got.message.contains("whose last warm-up failed."));
}

/// [`classify`] re-runs on its own stored output on every read, so the cut
/// sentence must classify to itself rather than degrade to the generic notice
/// or grow a second retry note.
#[test]
fn reclassifying_a_cut_warm_up_sentence_is_a_no_op() {
    let raw = "agent `researcher` is bound to harness `claude-code`, whose last \
               warm-up failed: could not start `claude`.";
    let once = classify(raw).expect("classifies");
    let twice = classify(&once.message).expect("its own output classifies again");
    assert_eq!(once, twice);
}

/// tinysweeper (PR #2401): text that merely *quotes* the router's two
/// markers — without the connective phrase that actually joins them in a
/// real sentence — must not misclassify an unrelated failure as a harness
/// binding problem. A turn that reached a model and failed for some other
/// reason must never be told to go check a harness.
#[test]
fn a_lookalike_with_no_connective_is_not_classified() {
    let lookalike = "the tool returned: agent `researcher` is bound to harness `runner` \
                      in the example the user pasted, but the actual failure was a timeout.";
    assert!(
        classify(lookalike).is_none(),
        "no `, but ` / `, whose last warm-up failed: ` right after the harness name — not a real router sentence"
    );
}

#[test]
fn a_lookalike_with_the_wrong_connective_is_not_classified() {
    assert!(
        classify("agent `researcher` is bound to harness `runner` and that is all it says.")
            .is_none()
    );
}
