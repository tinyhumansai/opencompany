use super::provider_test_helpers_tests::*;
use super::*;
use crate::app::config::MapEnv;

// ---- boot-time platform credential status (issue #879) -----------------

/// The #879 tenant: nothing at all is set. One warning must name every
/// surface that silently failed closed, and both tiers, because the fix
/// differs between a cluster tenant and `docker compose`.
#[test]
fn no_platform_credential_warns_about_every_managed_surface() {
    let status = PlatformCredentialStatus::resolve(&MapEnv::default());
    assert!(!status.platform_identity);
    assert!(!status.all_wired());

    let warning = status.boot_warning().expect("a warning");
    assert!(warning.contains("no platform credential"), "{warning}");
    for expected in [
        "inference",
        "web_search",
        "media",
        crate::company::credentials::TOKEN_FILE_ENV,
        crate::company::credentials::API_KEY_ENV,
    ] {
        assert!(warning.contains(expected), "{expected} missing: {warning}");
    }
}

/// The trap this check exists for. A hosted tenant given the projected token
/// volume — the documented hosted mechanism, and the fix for #879 — resolves
/// inference and search but **not** media, because
/// [`media_backend_from_env`] reads only the static tier. Staying silent
/// here is what leaves an operator who has just supplied the credential
/// staring at a console that still says "Awaiting credential".
#[test]
fn projected_token_alone_warns_that_media_cannot_use_it() {
    let (_dir, token_path) = projected_token_file();
    let env = MapEnv::new([(crate::company::credentials::TOKEN_FILE_ENV, token_path)]);

    let status = PlatformCredentialStatus::resolve(&env);
    assert!(status.platform_identity);
    assert!(status.projected_tier);
    assert!(status.inference, "inference reads the projected tier");
    assert!(status.search, "search reads the projected tier");
    assert!(!status.media, "media reads only the static tier");
    assert!(!status.all_wired());

    let warning = status.boot_warning().expect("a warning");
    assert!(warning.contains("media"), "{warning}");
    assert!(
        warning.contains("OPENCOMPANY_MEDIA_KEY"),
        "the warning must name the variable that fixes it: {warning}"
    );
}

/// A projected token plus a media key wires everything, so boot says
/// nothing. Guards the check against crying wolf on a healthy deployment.
#[test]
fn projected_token_with_a_media_key_is_silent() {
    let (_dir, token_path) = projected_token_file();
    let env = MapEnv::new([
        (
            crate::company::credentials::TOKEN_FILE_ENV.to_string(),
            token_path,
        ),
        (
            "OPENCOMPANY_MEDIA_KEY".to_string(),
            "media-specific".to_string(),
        ),
    ]);

    let status = PlatformCredentialStatus::resolve(&env);
    assert!(status.all_wired());
    assert_eq!(status.boot_warning(), None);
}

/// The `docker compose` / self-host shape: one static key feeds all three
/// surfaces, so boot is silent there too.
#[test]
fn a_static_key_wires_every_surface_and_is_silent() {
    let env = MapEnv::new([(crate::company::credentials::API_KEY_ENV, "th-static")]);
    let status = PlatformCredentialStatus::resolve(&env);

    assert!(status.platform_identity);
    assert!(!status.projected_tier);
    assert!(status.all_wired());
    assert_eq!(status.boot_warning(), None);
}

/// A media key on its own is not a platform identity: it wires media and
/// leaves inference and search closed, which the partial arm must report by
/// name rather than collapsing into "no credential".
#[test]
fn a_media_key_alone_warns_about_the_surfaces_it_does_not_cover() {
    let env = MapEnv::new([("OPENCOMPANY_MEDIA_KEY", "media-specific")]);
    let status = PlatformCredentialStatus::resolve(&env);

    assert!(!status.platform_identity);
    assert!(status.media);
    assert!(!status.inference);
    assert!(!status.search);

    let warning = status.boot_warning().expect("a warning");
    assert!(warning.contains("partly configured"), "{warning}");
    assert!(warning.contains("web_search"), "{warning}");
    assert!(
        !warning.contains("no platform credential"),
        "a partial deployment is not a bare one: {warning}"
    );
}

#[tokio::test]
async fn mock_provider_echoes_last_user_message_with_prefix() {
    let provider = MockProvider::new("reply: ");
    let out = provider.invoke(&(), user_request("hello")).await.unwrap();
    assert_eq!(out.text(), "reply: hello");
    assert_eq!(provider.telemetry_provider_id(), "mock");
}

#[tokio::test]
async fn mock_provider_ignores_system_and_echoes_last_user() {
    let provider = MockProvider::default();
    let req = ModelRequest {
        messages: vec![Message::system("be terse"), Message::user("ping")],
        model: Some("any".to_string()),
        ..Default::default()
    };
    let out = provider.invoke(&(), req).await.unwrap();
    assert_eq!(out.text(), "mock: ping");
}

#[test]
fn hosted_provider_reports_managed_telemetry_id() {
    let provider = HostedProvider::new(HostedProviderConfig {
        base_url: "https://example.test/v1".to_string(),
        credential: Credential::None,
        extra_headers: Vec::new(),
    });
    assert_eq!(provider.telemetry_provider_id(), "subscription");
}

/// The exact `/openai/v1` staging response shape: reply text plus a standard
/// `usage` block with a `prompt_tokens_details.cached_tokens` field and no
/// openhuman billing envelope. No charge ⇒ no `openhuman_usage_meta` key.
#[test]
fn parses_openai_v1_completion_with_usage() {
    let payload = serde_json::json!({
        "model": "chat-v1",
        "choices": [{ "message": { "role": "assistant", "content": "pong" } }],
        "usage": {
            "prompt_tokens": 22,
            "completion_tokens": 2,
            "total_tokens": 24,
            "prompt_tokens_details": { "cached_tokens": 5 }
        }
    });
    let resp = model_response_from_payload(payload).expect("parses");
    assert_eq!(resp.text(), "pong");
    assert!(resp.message.tool_calls.is_empty());
    let usage = resp.usage.expect("usage present");
    assert_eq!(usage.input_tokens, 22);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(usage.cache_read_tokens, 5);
    // No billing envelope → raw carries the wire payload but no meta key.
    assert!(
        resp.raw
            .as_ref()
            .unwrap()
            .get(OPENHUMAN_USAGE_META_KEY)
            .is_none(),
        "billing-free response must not fabricate a charge"
    );
}

/// Wire responses are freshly served unless the provider explicitly
/// reports otherwise. Keep the compatibility default introduced with the
/// TinyAgents response field pinned at this parsing boundary.
#[test]
fn parsed_response_is_not_marked_as_cached() {
    let payload = serde_json::json!({
        "choices": [{ "message": { "content": "fresh" } }]
    });

    let response = model_response_from_payload(payload).expect("parses");

    assert!(!response.served_from_cache);
}

/// The managed envelope wins for cached tokens and carries the USD charge,
/// which must survive onto `raw.openhuman_usage_meta.charged_amount_usd` so
/// the host cost layer bills it. This is the #1 billing-preservation contract.
#[test]
fn managed_envelope_supplies_cost_and_cached_tokens() {
    let payload = serde_json::json!({
        "choices": [{ "message": { "content": "ok" } }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 40,
            "prompt_tokens_details": { "cached_tokens": 1 }
        },
        "openhuman": {
            "usage": { "cached_input_tokens": 64 },
            "billing": { "charged_amount_usd": 0.0123 }
        }
    });
    let resp = model_response_from_payload(payload).expect("parses");
    let usage = resp.usage.expect("usage present");
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 40);
    assert_eq!(
        usage.cache_read_tokens, 64,
        "envelope beats prompt_tokens_details"
    );
    // The charged USD is re-projected onto raw for openhuman's cost pipeline.
    let charged = resp
        .raw
        .as_ref()
        .and_then(|raw| raw.get(OPENHUMAN_USAGE_META_KEY))
        .and_then(|meta| meta.get("charged_amount_usd"))
        .and_then(serde_json::Value::as_f64)
        .expect("charged_amount_usd survives onto raw");
    assert!(
        (charged - 0.0123).abs() < 1e-9,
        "charged_amount_usd must survive the billing envelope: {charged}"
    );
}

#[test]
fn empty_message_is_an_error_and_no_usage_is_none() {
    // Neither content nor tool_calls → genuinely empty, still an error.
    let empty = serde_json::json!({ "choices": [{ "message": {} }] });
    assert!(model_response_from_payload(empty).is_err());

    let no_usage = serde_json::json!({
        "choices": [{ "message": { "content": "hi" } }]
    });
    let resp = model_response_from_payload(no_usage).expect("parses");
    assert!(resp.usage.is_none());
}

/// Some OpenAI-compatible providers return `content` as an array of parts
/// (`[{"type":"text","text":"…"}]`) rather than a bare string. The parser
/// must concatenate the `text` of each text part instead of treating the
/// non-string value as empty and hard-erroring. Regression for bug #1.
#[test]
fn parses_content_as_array_of_text_parts() {
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Hello, " },
                    { "type": "text", "text": "world" }
                ]
            }
        }]
    });
    let resp = model_response_from_payload(payload).expect("array content parses");
    assert_eq!(resp.text(), "Hello, world");
    assert!(resp.message.tool_calls.is_empty());
}

/// A non-null but empty visible content field is not the documented
/// reasoning-only shape. It must not cause internal reasoning to be
/// promoted as the assistant answer.
#[test]
fn empty_string_content_does_not_fall_back_to_reasoning() {
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": "",
                "reasoning": "internal thought"
            }
        }]
    });
    let err = model_response_from_payload(payload)
        .expect_err("empty string content must not promote reasoning");
    assert!(err.to_string().contains("neither"));
}

/// An absent visible content field is distinct from an explicit null and
/// must not activate the reasoning-only fallback.
#[test]
fn absent_content_does_not_fall_back_to_reasoning() {
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "reasoning": "internal thought"
            }
        }]
    });
    let err = model_response_from_payload(payload)
        .expect_err("absent content must not promote reasoning");
    assert!(err.to_string().contains("neither"));
}

/// An unsupported content shape is not equivalent to null content.
#[test]
fn unsupported_content_does_not_fall_back_to_reasoning() {
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": [{ "type": "image_url", "image_url": {} }],
                "reasoning": "internal thought"
            }
        }]
    });
    let err = model_response_from_payload(payload)
        .expect_err("unsupported content must not promote reasoning");
    assert!(err.to_string().contains("neither"));
}

/// A reasoning-only turn returns `content: null` with the visible text under
/// a `reasoning` field and no tool calls. This used to fall back to the
/// reasoning text and parse as a successful reply — the managed reasoning
/// brain (deepseek/qwen via OpenRouter) hits this shape routinely, and the
/// fallback's premise was that the model's real answer had landed in the
/// wrong field.
///
/// That premise was wrong: `reasoning` is chain-of-thought, not a response.
/// Promoting it put raw, often mid-sentence deliberation in front of
/// operators as the agent's genuine reply, and because it was persisted and
/// replayed as real history, later turns had no actual answer to build on
/// and fabricated one instead of surfacing the gap (reasoning-leak repro,
/// live multi-agent chat). This shape must now error — the same
/// diagnosable empty-turn error every other no-real-answer shape returns —
/// so the harness retries automatically instead of showing leaked
/// deliberation as a final answer.
#[test]
fn reasoning_only_turn_errors_instead_of_promoting_reasoning_text() {
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": null,
                "reasoning": "The answer is 42."
            }
        }]
    });
    let err = model_response_from_payload(payload)
        .expect_err("reasoning-only turn must not promote reasoning into content");
    let msg = err.to_string();
    assert!(
        msg.contains("neither"),
        "must be the diagnosable empty-turn error, got: {msg}"
    );
    assert!(
        !msg.contains("42"),
        "leaked reasoning text must not appear in the error, got: {msg}"
    );
}

/// A reasoning trace that *mentions* a call in JSON shape has not requested
/// it, and running it would execute a thought rather than an instruction.
///
/// Reasoning is never promoted into `content` at all now, so this payload
/// — no visible message, no structured call — errors as an empty turn
/// before the text-tool-call recovery ever runs. That is a stronger
/// guarantee than the old "recovered but not dispatched" behavior: the
/// deliberation, call-shaped JSON included, never reaches the caller in
/// any form (Codex review on #2011, superseded by the reasoning-leak fix).
#[test]
fn a_tool_call_shape_inside_reasoning_is_never_recovered_or_leaked() {
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": null,
                "reasoning": "I could answer this by calling \
                              {\"call\":\"read_ledger\",\"arguments\":{\"ledger\":\"tasks\"}} \
                              but let me think about it first."
            }
        }]
    });
    let offered = std::collections::BTreeSet::from(["read_ledger".to_string()]);
    let err =
        model_response_from_payload_offering(payload, &offered, &std::collections::BTreeMap::new())
            .expect_err("a reasoning-only turn must not parse as success");
    let msg = err.to_string();
    assert!(
        msg.contains("neither"),
        "must be the diagnosable empty-turn error, got: {msg}"
    );
    assert!(
        !msg.contains("read_ledger"),
        "a deliberation must never be dispatched OR leaked into the error: {msg}"
    );
}

/// A response the model did not choose to end is a fragment, so a balanced
/// object inside it is not a completed request — the same reason the
/// reasoning fallback is gated on a finish reason (Codex review on #2011).
///
/// Includes `error`, which the guard names nowhere: the point of an
/// allow-list is that an unrecognized reason fails closed on its own.
#[test]
fn a_call_inside_a_truncated_response_is_not_recovered() {
    let offered = std::collections::BTreeSet::from(["read_ledger".to_string()]);
    // `error` is the unrecognized-reason case: named nowhere in the guard,
    // and refused because the guard allow-lists rather than blocklists
    // (CodeRabbit review on #2011).
    for reason in ["length", "content_filter", "failed", "error"] {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": reason,
                "message": {
                    "role": "assistant",
                    "content": "Checking now. {\"call\":\"read_ledger\",\
                                \"arguments\":{\"ledger\":\"tasks\"}}"
                }
            }]
        });
        let resp = model_response_from_payload_offering(
            payload,
            &offered,
            &std::collections::BTreeMap::new(),
        );
        let calls = resp.map(|r| r.message.tool_calls.len()).unwrap_or(0);
        assert_eq!(
            calls, 0,
            "a `{reason}` stop must not dispatch a recovered call"
        );
    }
}

/// A gateway that echoes its refusal into **both** `message.refusal` and the
/// visible `content` leaves the substituted value equal to the original, so
/// comparing text against a snapshot reads "untouched" for the one case that
/// most needs to block. Provenance is tracked instead (Codex review on
/// #2011).
#[test]
fn a_refusal_duplicated_into_content_still_blocks_recovery() {
    let refusal = "I can't do that. It would mean \
                   {\"call\":\"read_ledger\",\"arguments\":{\"ledger\":\"tasks\"}}";
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": refusal,
                "refusal": refusal
            }
        }]
    });
    let offered = std::collections::BTreeSet::from(["read_ledger".to_string()]);
    let resp =
        model_response_from_payload_offering(payload, &offered, &std::collections::BTreeMap::new())
            .expect("the refusal turn still parses");

    assert!(
        resp.message.tool_calls.is_empty(),
        "an action must never be recovered out of a refusal"
    );
}

/// The batch rule the parsed path enforces must hold for a recovered batch
/// too: `request_approval` beside any sibling is refused whole.
///
/// Without it, a text response pairing an approval request with an effectful
/// call would cross the approval boundary here that it cannot cross when the
/// same pair arrives as a structured array — and the policy fold only
/// refuses calls sequenced *after* the approval, so a sibling ordered before
/// it would run before the pending flag is set (Codex review on #2011).
#[test]
fn a_recovered_batch_pairing_request_approval_with_a_sibling_is_refused() {
    let approval = crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND;
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": format!(
                    "Doing both now. {{\"call\":\"read_ledger\",\"arguments\":{{\"ledger\":\"tasks\"}}}} \
                     and {{\"call\":\"{approval}\",\"arguments\":{{\"reason\":\"ship it\"}}}}"
                )
            }
        }]
    });
    let offered =
        std::collections::BTreeSet::from(["read_ledger".to_string(), approval.to_string()]);
    let err =
        model_response_from_payload_offering(payload, &offered, &std::collections::BTreeMap::new())
            .expect_err("the whole recovered batch must be refused");

    assert!(
        err.to_string().contains("approval boundary"),
        "refused for the wrong reason: {err}"
    );
}

#[test]
fn a_recovered_call_keeps_its_name_when_the_wire_offers_it_natively() {
    let payload = || {
        serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "Checking. {\"call\":\"read\",\"arguments\":{\"limit\":5}}"
                }
            }]
        })
    };
    let offered = std::collections::BTreeSet::from(["read".to_string()]);
    let native = std::collections::BTreeMap::from([(
        "read".to_string(),
        serde_json::json!({ "type": "object", "properties": { "limit": { "type": "integer" } } }),
    )]);
    let resp = model_response_from_payload_offering(payload(), &offered, &native)
        .expect("the recovered call parses");
    let calls = &resp.message.tool_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].name, "read",
        "a belt tool is dispatched by its own name"
    );
    assert_eq!(calls[0].arguments["limit"], 5);

    let resp = model_response_from_payload_offering(
        payload(),
        &offered,
        &std::collections::BTreeMap::new(),
    )
    .expect("the recovered call parses");
    let calls = &resp.message.tool_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].name, "mcp_call_tool",
        "a name offered only by the MCP brief is still bridged"
    );
    assert_eq!(calls[0].arguments["tool"], "read");
}

/// A refusal turn: `content: null`, `finish_reason: "stop"`, a nonempty
/// `message.refusal`, and `reasoning` the model emitted before declining.
/// The refusal is the provider's own visible safety response and must win
/// over the internal reasoning — promoting the reasoning instead would
/// expose exactly the content the model declined to return (CodeRabbit
/// review on #1779, comment 3872084054).
#[test]
fn a_refusal_wins_over_leaked_reasoning() {
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": null,
                "reasoning": "The user wants help with something I should decline.",
                "refusal": "I can't help with that."
            }
        }]
    });
    let resp = model_response_from_payload(payload).expect("refusal turn parses");
    assert_eq!(resp.text(), "I can't help with that.");
    assert!(resp.message.tool_calls.is_empty());
}

/// A refusal turn where the array-shaped `content` field itself encodes
/// the refusal as a `{"type":"refusal","refusal":"…"}` part instead of
/// the scalar sibling `message.refusal` field — some providers/gateways
/// normalize a Responses-API-style refusal part into the Chat
/// Completions `content` array. `extract_content_text` only concatenates
/// `"text"`-typed parts, so the refusal part contributes nothing and
/// `content` comes back empty; without an array-aware refusal check the
/// scalar `message.refusal` lookup also finds nothing, and the reasoning
/// fallback would promote the leaked pre-refusal reasoning as the
/// visible answer. The refusal must still win (Codex review on #1779,
/// comment 3874381270).
#[test]
fn a_refusal_wins_over_leaked_reasoning_when_refusal_is_an_array_content_part() {
    let payload = serde_json::json!({
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "refusal", "refusal": "I can't help with that." }
                ],
                "reasoning": "The user wants help with something I should decline."
            }
        }]
    });
    let resp = model_response_from_payload(payload).expect("refusal turn parses");
    assert_eq!(resp.text(), "I can't help with that.");
    assert!(resp.message.tool_calls.is_empty());
}
