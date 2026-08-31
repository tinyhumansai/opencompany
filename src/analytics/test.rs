//! The guarantee, tested: no analytics payload can carry caller-supplied text.
//!
//! These run at **default features**, in the build every lane compiles, because
//! the leak they guard against would not be introduced in the gated transport —
//! it would be introduced at a call site, in a payload field, on any build.

use super::*;
use crate::analytics::types::{OpaqueId, TenantIdKey, provider_slug, sample_kind_slug};
use crate::app::deployment::Deployment;
use crate::error::OpenCompanyError;
use crate::metering::ModelSlug;
use crate::ports::brain::{Cognition, UsageMetering};
use crate::ports::usage::{SampleKind, UsageSample};

/// Strings that must never appear in a payload, standing in for the four kinds
/// of content #1739 names: a customer's brand, an operator's own text, a host
/// path, and an address.
const HOSTILE: &[&str] = &[
    "AcmeCorp Holdings",
    "please summarise the merger memo",
    "/Users/someone/companies/acme/secrets",
    "founder@acme.example",
    "sk-not-a-real-key",
    "project-titan",
];

fn envelope() -> Envelope {
    Envelope::new(
        OpaqueId::instance("0123456789abcdef0123456789abcdef"),
        Deployment::HostedTenant,
        Cognition {
            path: "harness",
            provider: "openrouter",
            model: None,
            metering: UsageMetering::PerTurn,
        },
    )
}

/// Every event kind, each built from the most hostile input its constructor
/// will accept.
fn hostile_events() -> Vec<Event> {
    let sample = UsageSample {
        at_millis: 1,
        agent: "AcmeCorp Holdings".into(),
        provider: "mcp:project-titan".into(),
        input_tokens: 10,
        output_tokens: 4,
        cached_input_tokens: 0,
        cost_usd: 0.25,
        kind: SampleKind::Inference,
        run_id: Some("/Users/someone/companies/acme/secrets".into()),
        // A BYOK tenant names its model whatever it likes, and what it likes is
        // often its own brand. `ModelSlug::classify` folds it to `other` at the
        // harness; this sample proves the analytics payload cannot undo that.
        model: Some(ModelSlug::classify("AcmeCorp Holdings project-titan")),
    };

    let err = OpenCompanyError::Store(
        "could not write /Users/someone/companies/acme/secrets for founder@acme.example".into(),
    );

    // A second sample whose provider is neither MCP-prefixed nor a known slug:
    // it must reach the `other` fallback rather than the `mcp` branch. Without
    // it the whole fallback path went untested by the two assertions below —
    // found by mutating `provider_slug`'s `_` arm into a `Box::leak`, which the
    // MCP-prefixed sample alone did not notice.
    let unknown_provider = UsageSample {
        provider: "AcmeCorp Holdings".into(),
        kind: SampleKind::OauthCall,
        model: None,
        ..sample.clone()
    };

    // A third sample on the happy path: a model the vocabulary *can* name. The
    // two above only ever reach `other`, so on their own they would let the
    // `model` property be deleted outright without a payload changing.
    let named_model = UsageSample {
        model: Some(ModelSlug::classify("anthropic/claude-sonnet-4-6")),
        ..sample.clone()
    };

    vec![
        Event::InstanceStarted {
            companies: 3,
            storage: "mongodb",
            setup_complete: true,
        },
        Event::TurnFinished {
            trigger: Trigger::OperatorMessage,
            outcome: Outcome::Failed,
            failure: Some(FailureCode::of(&err)),
            duration_ms: 1_234,
            effects_executed: 2,
            approvals_parked: 1,
        },
        Event::metered(&sample),
        Event::metered(&unknown_provider),
        Event::metered(&named_model),
    ]
}

/// **Issue #1739's third acceptance criterion**, from the outside: render every
/// event from hostile inputs and assert none of that text survives.
///
/// It is the blunt half of the guarantee. The structural half is that
/// [`PropValue`] has no `String` variant at all — but a blunt test is what
/// fails loudly if someone adds one, so both are here.
#[test]
fn a_payload_carries_no_caller_supplied_text() {
    let envelope = envelope();
    for event in hostile_events() {
        // Case-insensitively: a classifier that lowercases what it passes
        // through has still passed it through, and an exact-case search would
        // read that as clean. Found by mutation — the first version of this
        // test missed exactly that.
        let rendered = payload(&envelope, &event).to_string().to_ascii_lowercase();
        for needle in HOSTILE {
            assert!(
                !rendered.contains(&needle.to_ascii_lowercase()),
                "the {} payload leaked {needle:?}: {rendered}",
                event.name()
            );
        }
    }
}

/// Every literal any classifier or enum in this module can produce.
///
/// The vocabulary is enumerated by hand **on purpose**: it is the list a
/// reviewer reads to answer "what can this product send?", and a list derived
/// from the code would answer that question with itself.
fn vocabulary() -> Vec<&'static str> {
    let mut words = vec![
        // Deployment kinds.
        "desktop",
        "self-hosted",
        "hosted-tenant",
        // Outcomes and triggers.
        "ok",
        "failed",
        "operator-message",
        "task-dispatch",
        "approval-continuation",
        "agent-reply",
        // Failures.
        "none",
        "store",
        "manifest",
        "refused",
        "not-found",
        "cognition",
        "workflow",
        "config",
        "upstream",
        // Metering.
        "per-turn",
        "per-cycle",
        // Cognition paths (`ports::brain::Cognition::path`).
        "harness",
        "hosted",
        "echo",
        "sidecar",
        "custom",
        // Storage kinds.
        "fs",
        "sqlite",
        "mongodb",
        // The catch-all.
        types::OTHER,
    ];
    // Provider slugs and sample kinds, taken from the classifiers themselves so
    // a new one cannot be added without appearing here.
    for provider in [
        "openrouter",
        "subscription",
        "managed",
        "ollama",
        "byok",
        "openai_compatible",
        "echo",
        "hosted",
        "github",
        "google",
        "slack",
        "notion",
        "composio",
        "unknown",
        "mcp:anything",
        "something nobody anticipated",
    ] {
        words.push(provider_slug(provider));
    }
    for kind in [
        SampleKind::Inference,
        SampleKind::OauthCall,
        SampleKind::SearchCall,
        SampleKind::PlanningCall,
        SampleKind::TriageCall,
        SampleKind::SetupCall,
        SampleKind::AuthoringCall,
        SampleKind::SelectorCall,
    ] {
        words.push(sample_kind_slug(kind));
    }
    // Model slugs, taken from `ModelSlug::classify` for the same reason the
    // provider slugs are taken from `provider_slug`: this module owns no model
    // vocabulary of its own — it forwards the one `crate::metering::model`
    // already folded a raw name onto, and a second hand-written copy here would
    // be a second place for the two to disagree.
    for model in [
        "anthropic/claude-sonnet-4-6",
        "chat-v1",
        "AcmeCorp Holdings project-titan",
    ] {
        words.push(ModelSlug::classify(model).as_str());
    }
    words
}

/// The structural claim, asserted rather than described: **every string value in
/// every payload is either the opaque identity, a platform fact fixed at compile
/// time, or a word from the vocabulary above.**
///
/// This is the test that fails the moment someone adds a `String`-carrying
/// property. A free-form value has, by definition, no entry in a hand-written
/// list.
#[test]
fn every_string_in_a_payload_comes_from_the_compiled_vocabulary() {
    let envelope = envelope();
    let vocabulary = vocabulary();

    // The three values that are strings but are not vocabulary: the opaque id,
    // and the two platform facts `std::env::consts` supplies. All three are
    // fixed for the life of the process and none originates with a user.
    let allowed_platform = [
        envelope.id.as_str().to_string(),
        envelope.app_version.to_string(),
        envelope.os.to_string(),
        envelope.arch.to_string(),
    ];

    for event in hostile_events() {
        let rendered = payload(&envelope, &event);
        assert_eq!(rendered["event"], event.name());

        let properties = rendered["properties"]
            .as_object()
            .expect("properties is an object");
        assert!(!properties.is_empty(), "an event with no properties");

        for (key, value) in properties {
            let Some(text) = value.as_str() else { continue };
            let known = vocabulary.contains(&text)
                || allowed_platform.iter().any(|allowed| allowed == text);
            assert!(
                known,
                "the property {key:?} carried the string {text:?}, which is not in this \
                 module's compiled vocabulary. Either it is a leak, or a new literal was \
                 added without recording it in `vocabulary()`."
            );
        }
    }
}

/// A tenant slug is the customer's brand. It is derived under a key before it
/// leaves, never carried, and the derived value is stable so that uniques and
/// funnels still mean something.
#[test]
fn a_tenant_slug_is_derived_rather_than_carried() {
    let key = TenantIdKey::new("not-a-real-id-key").expect("a non-blank key");
    let id = OpaqueId::tenant("acmecorp-holdings", &key);
    assert!(!id.as_str().contains("acme"), "{id:?}");
    assert!(
        id.as_str().starts_with("t_"),
        "tenant ids are namespaced apart from instance ids: {id:?}"
    );
    assert_eq!(
        id.as_str(),
        OpaqueId::tenant("acmecorp-holdings", &key).as_str(),
        "the same tenant must map to the same id on every boot, or uniques and \
         funnels mean nothing"
    );
    assert_ne!(
        id.as_str(),
        OpaqueId::tenant("acmecorp-holdings-2", &key).as_str()
    );
}

/// **The tenant id must not be invertible by whoever holds it.**
///
/// Not carrying the brand as a substring — the only thing the test above
/// checks — is a much weaker property than it looks. A tenant slug is usually
/// the customer's brand: a small, public, enumerable set. Under the plain
/// `SHA-256(slug)` this used to use, the collector (or anyone with access to
/// the analytics project) could hash a few thousand candidate brands and read
/// `t_<digest>` straight back to the customer, so "opaque" was not true of it
/// in any useful sense.
///
/// The guard is that the id is not a function of the slug alone. The self-check
/// underneath is the load-bearing half: it recomputes the **old** unkeyed
/// digest and proves an enumerating attacker's guess really would have matched,
/// so this is testing the fix rather than restating the implementation.
#[test]
fn a_tenant_id_is_not_enumerable_from_the_slug() {
    use sha2::{Digest, Sha256};

    let slug = "acmecorp-holdings";
    let key = TenantIdKey::new("not-a-real-id-key").expect("a non-blank key");
    let id = OpaqueId::tenant(slug, &key);

    // What an attacker who knows only the slug can compute: the old
    // construction, exactly as it shipped — SHA-256, first 16 bytes, hex.
    let guessed: String = Sha256::digest(slug.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();

    // The self-check: that guess is well-formed and really is what the old
    // construction produced, so the assertion below is not vacuous.
    assert_eq!(guessed.len(), 32, "the guess must be a full 128-bit digest");
    assert_eq!(
        format!("t_{guessed}").len(),
        id.as_str().len(),
        "the guess must be the same shape as the id, or it could never have \
         matched for reasons that have nothing to do with the key"
    );

    assert_ne!(
        id.as_str(),
        format!("t_{guessed}"),
        "the tenant id is derivable from the slug alone, so anyone holding it \
         can enumerate candidate brands and identify the customer"
    );

    // And the key is what makes the difference: change only the key and the id
    // changes, which is the property an enumerating attacker cannot work around
    // without it.
    let other = TenantIdKey::new("not-a-real-id-key-either").expect("a non-blank key");
    assert_ne!(
        id.as_str(),
        OpaqueId::tenant(slug, &other).as_str(),
        "the same slug under a different key must not collide"
    );
}

/// A key is a credential: it must not be printable by accident, because the
/// accident is a `{:?}` in a log line nobody reviewed. Asserted
/// case-insensitively, with the self-check, for the reason the endpoint guards
/// carry one.
#[test]
fn a_tenant_id_key_is_not_printable() {
    const SECRET: &str = "NotARealTenantIdKey";
    let key = TenantIdKey::new(SECRET).expect("a non-blank key");
    let printed = format!("{key:?}");
    assert!(
        !printed
            .to_ascii_lowercase()
            .contains(&SECRET.to_ascii_lowercase()),
        "the Debug impl leaked the key: {printed}"
    );
    // The self-check, against a value that actually carries the key: the same
    // search *does* find it in an unredacted rendering, so the assertion above
    // is refusing something findable rather than passing because the needle
    // could never be found at all. Comparing the constant with itself proved
    // only that `contains` is reflexive.
    let unredacted = format!("TenantIdKey({SECRET})");
    assert!(
        unredacted
            .to_ascii_lowercase()
            .contains(&SECRET.to_ascii_lowercase()),
        "the needle must be findable in an unredacted rendering, or the guard above is vacuous"
    );
}

/// A blank key is no key. It must read as *absent* — sending the caller to the
/// random instance id — rather than as a key, because a key everybody can guess
/// is the enumerable digest all over again.
#[test]
fn a_blank_tenant_id_key_is_no_key() {
    for blank in ["", "   ", "\n", "\t\n "] {
        assert_eq!(TenantIdKey::new(blank), None, "{blank:?}");
    }
    assert!(TenantIdKey::new("  not-a-real-id-key\n").is_some());
}

/// `Display` on this crate's error type embeds absolute paths, company ids, tool
/// names and agent text — it is the richest source of user content in the tree.
/// A failure property is the coarse class and nothing else.
#[test]
fn an_error_reaches_a_payload_only_as_a_coarse_class() {
    let err =
        OpenCompanyError::Store("could not write /Users/someone/companies/acme/secrets".into());
    assert!(
        err.to_string().contains("/Users/someone"),
        "the premise of this test: Display really does carry the path"
    );
    assert_eq!(FailureCode::of(&err), FailureCode::Store);
    assert_eq!(FailureCode::of(&err).as_str(), "store");

    // And an upstream's own code is folded to the family, never carried.
    let upstream = OpenCompanyError::Chargebee {
        status: 404,
        code: "customer_acme_holdings_not_found".into(),
        message: "nope".into(),
    };
    assert_eq!(FailureCode::of(&upstream), FailureCode::Upstream);
    assert!(!FailureCode::of(&upstream).as_str().contains("acme"));
}

/// An unrecognised provider is folded to `other`, not passed through. This is
/// the direction that matters: the leak would arrive as a value nobody
/// anticipated, which is the only way such a leak ever arrives.
#[test]
fn an_unknown_provider_folds_to_other() {
    assert_eq!(provider_slug("mcp:acme-internal-crm"), "mcp");
    assert_eq!(provider_slug("acme-internal-crm"), types::OTHER);
    assert_eq!(provider_slug("OpenRouter"), "openrouter");
}

/// The point of #1749 reaching analytics at all: "which model is this fleet's
/// spend going to?" is answerable from `turn_metered` alone. `provider` names
/// *who served* the tokens, so on a subscription tenant every sample says
/// `subscription` whichever of four workloads produced it.
///
/// Fails if the `("model", …)` push in `Event::props` is deleted — the
/// mutation that would otherwise leave every other assertion here green.
#[test]
fn a_metered_event_names_the_model_it_spent_on() {
    let sample = UsageSample {
        at_millis: 1,
        agent: "maya".into(),
        provider: "subscription".into(),
        input_tokens: 10,
        output_tokens: 4,
        cached_input_tokens: 0,
        cost_usd: 0.25,
        kind: SampleKind::Inference,
        run_id: None,
        model: Some(ModelSlug::classify("anthropic/claude-sonnet-4-6")),
    };

    let rendered = payload(&envelope(), &Event::metered(&sample));
    assert_eq!(
        rendered["properties"]["model"], "anthropic-sonnet",
        "the slug the harness already classified, forwarded verbatim: {rendered}"
    );
}

/// A sample that named no model omits the property rather than reporting a
/// word for it. `other` means "a model ran that this build cannot name", and
/// an OAuth call that ran no model at all is a different fact — collapsing the
/// two would silently inflate the `other` bucket with every tool call.
///
/// Fails if the conditional push becomes an unconditional
/// `PropValue::Word(model.unwrap_or(OTHER))` — or `.unwrap_or("none")`.
#[test]
fn a_sample_with_no_model_carries_no_model_property() {
    let sample = UsageSample {
        at_millis: 1,
        agent: "maya".into(),
        provider: "github".into(),
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        cost_usd: 0.0,
        kind: SampleKind::OauthCall,
        run_id: None,
        model: None,
    };

    let rendered = payload(&envelope(), &Event::metered(&sample));
    let properties = rendered["properties"]
        .as_object()
        .expect("properties is an object");
    assert!(
        !properties.contains_key("model"),
        "no model ran, so there is nothing to say: {rendered}"
    );
    assert_eq!(properties["sample_kind"], "oauth-call");
}

/// The whole reason the property is a [`ModelSlug`] and not the raw name: a
/// BYOK or `openai_compatible` tenant can call its model anything, including
/// its own brand, and that name is folded at the harness before it is ever
/// stored.
#[test]
fn a_byok_model_name_reaches_the_payload_only_as_other() {
    assert_eq!(
        ModelSlug::classify("AcmeCorp Holdings project-titan").as_str(),
        types::OTHER,
        "the premise: an unrecognised model folds to the catch-all"
    );

    let sample = UsageSample {
        at_millis: 1,
        agent: "maya".into(),
        provider: "openai_compatible".into(),
        input_tokens: 10,
        output_tokens: 4,
        cached_input_tokens: 0,
        cost_usd: 0.25,
        kind: SampleKind::Inference,
        run_id: None,
        model: Some(ModelSlug::classify("AcmeCorp Holdings project-titan")),
    };

    let rendered = payload(&envelope(), &Event::metered(&sample))
        .to_string()
        .to_ascii_lowercase();
    assert!(!rendered.contains("acmecorp"), "{rendered}");
    assert!(!rendered.contains("project-titan"), "{rendered}");
    assert!(rendered.contains(types::OTHER), "{rendered}");
}

/// The default tracker in every build sends nothing and records nothing.
#[tokio::test]
async fn the_null_tracker_is_a_no_op() {
    let tracker = null_tracker();
    tracker.track(Event::InstanceStarted {
        companies: 1,
        storage: "fs",
        setup_complete: false,
    });
    tracker.flush().await;
}

/// A default build resolves to silence for the two deployments that must never
/// report, whatever else is configured. The transport-level proof is in
/// `mixpanel.rs`; this is the same decision asserted where every lane runs it.
#[test]
fn the_default_build_chooses_silence_for_desktop_and_self_hosted() {
    use crate::analytics::config::{Silence, TOKEN_ENV};
    use crate::app::config::MapEnv;

    let env = MapEnv::new([(TOKEN_ENV, "not-a-real-token")]);
    for deployment in [Deployment::Desktop, Deployment::SelfHosted] {
        assert_eq!(
            resolve(deployment, &env),
            Decision::Silent(Silence::NotHosted),
            "{deployment:?} must be silent"
        );
    }
}

/// **The deferred handle holds what it is given before installation and replays
/// it, in order, when the real tracker arrives.**
///
/// It used to drop those events, on the reasoning that the pre-install window
/// at boot contains nothing. It does not: `CompanyScheduler::spawn` runs its
/// restart catch-up immediately, so a company with a cron occurrence missed
/// during downtime finishes a real cycle inside that window, and its
/// `turn_finished` and `turn_metered` went nowhere.
#[tokio::test]
async fn a_deferred_tracker_holds_before_install_and_forwards_after() {
    let event = |companies| Event::InstanceStarted {
        companies,
        storage: "fs",
        setup_complete: false,
    };

    let deferred = DeferredTracker::new();
    deferred.track(event(1));
    deferred.track(event(2));

    let recorder = std::sync::Arc::new(RecordingTracker::new());
    assert!(deferred.install(recorder.clone()));
    deferred.track(event(3));
    deferred.flush().await;

    assert_eq!(
        recorder.events(),
        vec![event(1), event(2), event(3)],
        "both held events arrive, in order, ahead of the one tracked after"
    );
    assert_eq!(recorder.flushes(), 1);

    // A second install is refused rather than splitting the stream in two.
    let second = std::sync::Arc::new(RecordingTracker::new());
    assert!(!deferred.install(second.clone()));
    deferred.track(event(4));
    assert!(second.events().is_empty());
    assert_eq!(recorder.events().len(), 4);
}

/// **A host relabels its cognition when boot's answer stops being true.**
///
/// Boot reads the first registered runtime, so a hosted host provisioned into an
/// empty registry recorded `custom`/`unknown` and kept it for the life of the
/// process, and a company rebuilt in place after its first inference config
/// moved from `echo` to `harness` under an envelope that still said `echo`.
#[test]
fn an_envelope_relabels_its_cognition() {
    let mut envelope = Envelope::new(
        OpaqueId::instance("0123456789abcdef0123456789abcdef"),
        Deployment::HostedTenant,
        Cognition::default(),
    );
    let event = Event::InstanceStarted {
        companies: 0,
        storage: "fs",
        setup_complete: false,
    };

    // The premise: an unprovisioned host really does report the default.
    let before = payload(&envelope, &event);
    assert_eq!(before["properties"]["cognition_path"], "custom");
    assert_eq!(before["properties"]["cognition_provider"], "unknown");

    envelope.set_cognition(Cognition {
        path: "harness",
        provider: "openrouter",
        model: None,
        metering: UsageMetering::PerTurn,
    });

    let after = payload(&envelope, &event);
    assert_eq!(after["properties"]["cognition_path"], "harness");
    assert_eq!(after["properties"]["cognition_provider"], "openrouter");
    assert_eq!(after["properties"]["cognition_metering"], "per-turn");
}

/// And a relabel cannot widen what a payload may say: an unrecognised provider
/// folds to `other` on this path exactly as it does everywhere else.
#[test]
fn a_relabelled_cognition_still_goes_through_the_closed_vocabulary() {
    let mut envelope = envelope();
    envelope.set_cognition(Cognition {
        path: "harness",
        provider: "AcmeCorp Holdings",
        model: None,
        metering: UsageMetering::PerTurn,
    });
    let rendered = payload(
        &envelope,
        &Event::InstanceStarted {
            companies: 1,
            storage: "fs",
            setup_complete: true,
        },
    )
    .to_string()
    .to_ascii_lowercase();
    assert!(!rendered.contains("acmecorp"), "{rendered}");
    assert!(rendered.contains("other"), "{rendered}");
}

/// The port carries the observation through the deferred handle, which is how
/// it reaches the transport from `server::provision` and `runtime::rebuild`.
#[tokio::test]
async fn a_deferred_tracker_forwards_a_cognition_observation() {
    let cognition = Cognition {
        path: "harness",
        provider: "openrouter",
        model: None,
        metering: UsageMetering::PerTurn,
    };
    let deferred = DeferredTracker::new();
    let recorder = std::sync::Arc::new(RecordingTracker::new());
    assert!(deferred.install(recorder.clone()));

    assert_eq!(recorder.observed_cognition(), None);
    deferred.observe_cognition(cognition);
    assert_eq!(recorder.observed_cognition(), Some(cognition));
}

/// The held buffer is bounded. A handle nobody ever installs — every embedder
/// that wires no analytics — must not grow without limit, so the oldest are
/// dropped rather than the process.
#[tokio::test]
async fn the_held_buffer_is_bounded() {
    let event = |companies| Event::InstanceStarted {
        companies,
        storage: "fs",
        setup_complete: false,
    };

    let deferred = DeferredTracker::new();
    for n in 0..5_000u64 {
        deferred.track(event(n));
    }

    let recorder = std::sync::Arc::new(RecordingTracker::new());
    assert!(deferred.install(recorder.clone()));

    let held = recorder.events();
    assert!(
        held.len() < 5_000,
        "the buffer must be bounded, not unbounded: {}",
        held.len()
    );
    assert!(!held.is_empty(), "and it must not be zero either");
    assert_eq!(
        held.last(),
        Some(&event(4_999)),
        "the newest survives; it is the oldest that is dropped"
    );
}
