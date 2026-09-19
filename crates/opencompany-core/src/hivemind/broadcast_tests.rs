//! Tests for [`super::broadcast`]: semantic handoff over a stub router.
//!
//! The router is a fixture rather than a live Choice, so what is under test is
//! this host's half — provenance, candidate shape, and which ids a plan
//! assigns — and not the provider's judgement. `hivemind::typesafe` covers the
//! wire; nothing here reaches one.

use super::{Broadcast, candidate, recipients, route};
use tinyhivemind::responder::Probability;
use tinyhivemind_embed::routing::Router;
use tinyhivemind_embed::{
    CandidateProbability, RouterFuture, RoutingEvaluation, RoutingPlan, RoutingRequest,
};

/// Serialises every test that routes.
///
/// [`super::trace`] reads `OPENCOMPANY_DATA_DIR`, which is PROCESS-wide, and
/// `a_routing_decision_is_written_where_it_can_be_audited` sets it for its own
/// window. Any test routing concurrently inside that window writes its own
/// record into the same file, so an assertion about the first line reads
/// somebody else's decision — observed as a 1-in-3 failure claiming
/// `source == "desk_message"`. The variable is the shared resource, so the lock
/// belongs on everything that can write through it, not on the one test that
/// sets it.
static ROUTING: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Answers every request with one fixed distribution.
struct Fixture {
    evaluation: RoutingEvaluation,
}

impl Router for Fixture {
    fn evaluate<'a>(&'a self, _request: &'a RoutingRequest) -> RouterFuture<'a> {
        let evaluation = self.evaluation.clone();
        Box::pin(async move { Ok(evaluation) })
    }
}

/// A distribution naming `primary` at `primary_parts`, with the remainder spread
/// over `others` so the >20% rule has something to bite on.
///
/// The domain is **complete on purpose**: `valid_domain` requires one
/// probability for every eligible candidate plus `none`, and one contribution
/// per candidate. A partial fixture is not a weaker test, it is a rejected
/// evaluation that falls back to the deterministic responder — so every
/// assertion downstream would pass or fail for a reason that has nothing to do
/// with the rule under test. That is exactly how these tests failed first time.
fn evaluation(primary: &str, primary_parts: u32, others: &[(&str, u32)]) -> RoutingEvaluation {
    let parts_for = |id: &str| -> u32 {
        if id == primary {
            primary_parts
        } else {
            others
                .iter()
                .find(|(other, _)| *other == id)
                .map_or(0, |(_, parts)| *parts)
        }
    };

    let mut probabilities: Vec<CandidateProbability> = desk()
        .iter()
        .map(|candidate| CandidateProbability {
            probability: Probability::new(parts_for(&candidate.id)).expect("bounded"),
            candidate_id: candidate.id.clone(),
        })
        .collect();
    probabilities.push(CandidateProbability {
        candidate_id: "none".to_owned(),
        probability: Probability::ZERO,
    });

    let contributions: Vec<tinyhivemind_embed::ContributionProbability> = desk()
        .iter()
        .map(|candidate| tinyhivemind_embed::ContributionProbability {
            candidate_id: candidate.id.clone(),
            probability: Probability::ZERO,
        })
        .collect();
    RoutingEvaluation {
        primary_responder: primary.to_owned(),
        primary_probabilities: probabilities,
        confidence: Probability::new(900_000).expect("bounded"),
        needs_collaboration: Probability::ZERO,
        needs_clarification: Probability::ZERO,
        contributions,
        high_impact: Probability::ZERO,
        model_identity: "fixture".to_owned(),
        // Must match what the real router stamps, or acceptance rejects the
        // evaluation as a stale schema before the >20% rule is ever reached —
        // which would make every assertion below pass for the wrong reason.
        question_schema_version: 2,
        // Likewise must equal the request's, which `routed` sets to 1.
        roster_version: 1,
        disposition: tinyhivemind_embed::EvaluationDisposition::Unchecked,
    }
}

fn desk() -> Vec<tinyhivemind_embed::RouteCandidate> {
    vec![
        candidate(
            "checker",
            "Checker",
            Some("adversarial independent verification".to_owned()),
            None,
        ),
        candidate(
            "theory",
            "Theory",
            Some("algebraic structure and exact proofs".to_owned()),
            None,
        ),
        candidate(
            "lead",
            "Lead",
            Some("coordination and evidence synthesis".to_owned()),
            None,
        ),
    ]
}

async fn routed(evaluation: RoutingEvaluation) -> Vec<String> {
    route(
        &Fixture { evaluation },
        Broadcast {
            message: "N=10..20 matches direct interpolation; the degree-9 identity needs a proof.",
            author: "solver",
            desk_id: "pe1008",
            desk_purpose: Some("derive and verify the coefficient".to_owned()),
            candidates: desk(),
            roster_version: 1,
            fallback_responder: "lead",
        },
    )
    .await
}

#[tokio::test]
async fn a_confident_choice_reaches_exactly_one_teammate() {
    let _routing = ROUTING.lock().await;
    // Sums to exactly 1.0, as every distribution here must: acceptance rejects
    // a partial one and falls back, which is how this test first failed.
    // 0.94 on the winner leaves nobody else above the 20% threshold.
    let ids = routed(evaluation("checker", 940_000, &[("theory", 60_000)])).await;
    assert_eq!(ids, vec!["checker".to_owned()]);
}

#[tokio::test]
async fn an_uncertain_choice_widens_rather_than_gambling() {
    let _routing = ROUTING.lock().await;
    // Both alternatives clear 20%, so the round carries all three: uncertainty
    // is spent on reach, which is the whole point of the rule.
    let ids = routed(evaluation(
        "checker",
        400_000,
        &[("theory", 350_000), ("lead", 250_000)],
    ))
    .await;
    assert_eq!(ids[0], "checker", "the maximum is assigned first");
    assert!(ids.contains(&"theory".to_owned()));
    assert!(ids.contains(&"lead".to_owned()));
}

#[tokio::test]
async fn an_option_at_or_below_the_threshold_is_not_invited() {
    let _routing = ROUTING.lock().await;
    // Strictly above 20%, so exactly 20% does not qualify.
    let ids = routed(evaluation(
        "checker",
        700_000,
        &[("theory", 200_000), ("lead", 100_000)],
    ))
    .await;
    assert_eq!(ids, vec!["checker".to_owned()]);
}

#[tokio::test]
async fn a_broadcast_naming_its_own_author_falls_back_without_asking_a_model() {
    let _routing = ROUTING.lock().await;
    // `solver` is the author AND a candidate, which is a loop. The library
    // fails it closed; the deterministic destination stands in.
    struct NeverCalled;
    impl Router for NeverCalled {
        fn evaluate<'a>(&'a self, _request: &'a RoutingRequest) -> RouterFuture<'a> {
            panic!("invalid provenance must not spend a model call");
        }
    }

    let mut candidates = desk();
    candidates.push(candidate("solver", "Solver", None, None));

    let ids = route(
        &NeverCalled,
        Broadcast {
            message: "anything",
            author: "solver",
            desk_id: "pe1008",
            desk_purpose: None,
            candidates,
            roster_version: 1,
            fallback_responder: "lead",
        },
    )
    .await;

    assert_eq!(ids, vec!["lead".to_owned()], "the fallback is delivered");
}

#[test]
fn a_clarify_plan_assigns_nobody_so_the_caller_can_say_so() {
    let plan = RoutingPlan::Clarify {
        evaluation: evaluation("checker", 500_000, &[]),
    };
    assert!(recipients(&plan).is_empty());
}

#[test]
fn a_fallback_plan_is_a_delivery_not_a_silence() {
    let plan = RoutingPlan::Fallback {
        responder_id: "lead".to_owned(),
        reason: tinyhivemind_embed::RoutingFallback::InvalidBroadcast,
    };
    assert_eq!(recipients(&plan), vec!["lead".to_owned()]);
}

#[tokio::test]
async fn an_explicit_mention_outranks_routing_and_spends_no_model_call() {
    let _routing = ROUTING.lock().await;
    // A named teammate is the top rung of the responder ladder. Routing must
    // not be consulted at all — not consulted and then overridden.
    struct NeverCalled;
    impl Router for NeverCalled {
        fn evaluate<'a>(&'a self, _request: &'a RoutingRequest) -> RouterFuture<'a> {
            panic!("an explicit mention must short-circuit before the provider");
        }
    }

    let ids = super::route_desk_message(
        &NeverCalled,
        super::DeskMessage {
            message: "anyone got a view on the k=7 case?",
            desk_id: "pe1008",
            desk_purpose: None,
            candidates: desk(),
            roster_version: 1,
            explicit_responder: Some("theory"),
            fallback_responder: "lead",
        },
    )
    .await;

    assert_eq!(ids, vec!["theory".to_owned()]);
}

#[tokio::test]
async fn an_unaddressed_desk_message_is_routed_by_meaning() {
    let _routing = ROUTING.lock().await;
    let ids = super::route_desk_message(
        &Fixture {
            evaluation: evaluation("checker", 940_000, &[("theory", 60_000)]),
        },
        super::DeskMessage {
            message: "the degree-9 identity still needs an independent proof",
            desk_id: "pe1008",
            desk_purpose: Some("derive and verify the coefficient".to_owned()),
            candidates: desk(),
            roster_version: 1,
            explicit_responder: None,
            fallback_responder: "lead",
        },
    )
    .await;

    assert_eq!(ids, vec!["checker".to_owned()]);
}

#[tokio::test]
async fn an_uncertain_opening_route_names_a_room_rather_than_one_responder() {
    let _routing = ROUTING.lock().await;
    // `RoutingPlan::Hive`: the router deciding this message deserves a room.
    let ids = super::route_desk_message(
        &Fixture {
            evaluation: evaluation(
                "checker",
                400_000,
                &[("theory", 350_000), ("lead", 250_000)],
            ),
        },
        super::DeskMessage {
            message: "is the reduction sound?",
            desk_id: "pe1008",
            desk_purpose: None,
            candidates: desk(),
            roster_version: 1,
            explicit_responder: None,
            fallback_responder: "lead",
        },
    )
    .await;

    assert_eq!(ids[0], "checker", "the primary opens");
    assert!(ids.len() > 1, "and invitees join it: {ids:?}");
}

#[tokio::test]
async fn a_routing_decision_is_written_where_it_can_be_audited() {
    let _routing = ROUTING.lock().await;
    // The gap this closes: a call could be seen leaving the host and its
    // decision could not be reviewed afterwards — no distribution, no
    // confidence, no candidate list. A confident 0.94 and a coin-flip 0.34
    // produce the same visible outcome.
    let dir = std::env::temp_dir().join(format!("oc-route-trace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch root");
    // SAFETY: single-threaded within this test, and the variable is one this
    // process owns for the duration.
    unsafe {
        std::env::set_var("OPENCOMPANY_DATA_DIR", &dir);
    }

    let ids = routed(evaluation("checker", 940_000, &[("theory", 60_000)])).await;
    assert_eq!(ids, vec!["checker".to_owned()]);

    let written = std::fs::read_to_string(dir.join(super::TRACE_FILE)).expect("a trace line");
    let record: serde_json::Value =
        serde_json::from_str(written.lines().next().expect("one line")).expect("valid json");

    assert_eq!(record["source"], "broadcast");
    assert_eq!(record["recipients"][0], "checker");
    // The whole plan, so the distribution behind the pick is reviewable.
    assert!(
        record["plan"].to_string().contains("940000"),
        "the probability that decided it must survive: {record}"
    );

    unsafe {
        std::env::remove_var("OPENCOMPANY_DATA_DIR");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
