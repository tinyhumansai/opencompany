//! Tests for the progress pump's derived readings: cost off the stream,
//! per-attempt segmentation, and the iteration-cap tell.

use super::*;

fn cost(total_usd: f64, iteration: u32) -> AgentProgress {
    AgentProgress::TurnCostUpdated {
        model: "m".to_string(),
        iteration,
        input_tokens: 10 * u64::from(iteration),
        output_tokens: 5,
        cached_input_tokens: 0,
        total_usd,
    }
}

#[test]
fn the_last_cumulative_cost_is_the_turn_total() {
    let events = vec![AgentProgress::TurnStarted, cost(0.1, 1), cost(0.3, 2)];
    let usage = last_observed_turn_cost(&events).expect("cost");
    assert_eq!(usage.input_tokens, 20);
    assert!((usage.cost_usd - 0.3).abs() < f64::EPSILON);
    assert!(last_observed_turn_cost(&[AgentProgress::TurnStarted]).is_none());
}

#[test]
fn attempts_are_segmented_at_each_turn_start() {
    let events = vec![
        AgentProgress::TurnStarted,
        cost(0.1, 1),
        AgentProgress::TurnStarted,
        cost(0.5, 1),
    ];
    let segments = attempt_event_segments(&events, 3);
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0].len(), 2);
    assert_eq!(segments[1].len(), 2);
    assert!(
        segments[2].is_empty(),
        "an attempt that never started has no events"
    );
    assert!(
        (last_observed_turn_cost(segments[1]).expect("cost").cost_usd - 0.5).abs() < f64::EPSILON
    );
}

#[test]
fn a_turn_whose_last_iteration_is_the_cap_paused_at_it() {
    let capped = vec![
        AgentProgress::TurnStarted,
        AgentProgress::IterationStarted {
            iteration: 1,
            max_iterations: 2,
        },
        AgentProgress::IterationStarted {
            iteration: 2,
            max_iterations: 2,
        },
        AgentProgress::TurnCompleted { iterations: 2 },
    ];
    assert!(hit_iteration_cap(&capped));
    let finished = vec![
        AgentProgress::TurnStarted,
        AgentProgress::IterationStarted {
            iteration: 1,
            max_iterations: 25,
        },
        AgentProgress::TurnCompleted { iterations: 1 },
    ];
    assert!(!hit_iteration_cap(&finished));
    assert!(!hit_iteration_cap(&[]));
    // Only the LAST attempt counts: a capped first attempt followed by a
    // clean retry is a finished turn.
    let mut retried = capped.clone();
    retried.extend(finished.clone());
    assert!(!hit_iteration_cap(&retried));

    // The same two turns as `retried`, in the shape a *real* stream has: no
    // `TurnStarted` anywhere. The boundary must still be found, or the first
    // turn's cap leaks into the second and a finished turn reports a pause it
    // never took — which is exactly what reached CI.
    let real_capped = vec![
        AgentProgress::IterationStarted {
            iteration: 25,
            max_iterations: 25,
        },
        AgentProgress::TurnCompleted { iterations: 25 },
    ];
    let real_finished = vec![
        AgentProgress::IterationStarted {
            iteration: 1,
            max_iterations: 25,
        },
        AgentProgress::TurnCompleted { iterations: 1 },
    ];
    assert!(
        hit_iteration_cap(&real_capped),
        "a real capped turn must still be detected without a TurnStarted"
    );
    let mut real_retried = real_capped.clone();
    real_retried.extend(real_finished);
    assert!(
        !hit_iteration_cap(&real_retried),
        "the previous turn's TurnCompleted bounds the scan: this turn ran one \
         iteration of twenty-five and finished"
    );

    // Mid-turn, nothing completed yet: the cap belongs to the turn in flight.
    let mut real_inflight = real_capped.clone();
    real_inflight.extend(vec![AgentProgress::IterationStarted {
        iteration: 25,
        max_iterations: 25,
    }]);
    assert!(
        hit_iteration_cap(&real_inflight),
        "a turn still running at the ceiling has hit the cap"
    );
}

#[tokio::test]
async fn the_pump_returns_every_event_in_order_after_finish() {
    let pump = ProgressPump::start(StepLabels::default(), None, None);
    let tx = pump.sender();
    tx.send(AgentProgress::TurnStarted).await.expect("send");
    tx.send(AgentProgress::TurnCompleted { iterations: 1 })
        .await
        .expect("send");
    drop(tx);
    let events = pump.finish().await;
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], AgentProgress::TurnStarted));
}

/// #988's invariant, pinned where it can be checked without running a turn.
///
/// The end-to-end pair in `spend_halt_turn_tests` covers the same ground but
/// drives a live scripted turn, so it only catches a regression when timing
/// lines up — it passed locally five runs in a row while failing in CI. This
/// is the same rule with the timing removed.
#[test]
fn a_spend_halt_is_never_also_reported_as_a_step_pause() {
    assert!(
        reportable_iteration_cap(true, false),
        "a turn that only hit the cap reports it"
    );
    assert!(
        !reportable_iteration_cap(true, true),
        "a turn that hit the cap AND ran out of money reports the halt, not a \
         resumable pause — \"continue\" would invite spending a spent budget"
    );
    assert!(!reportable_iteration_cap(false, true));
    assert!(!reportable_iteration_cap(false, false));
}
