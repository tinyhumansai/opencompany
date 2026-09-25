//! The per-turn progress pump: OpenHuman's [`AgentProgress`] stream → live
//! console frames, the durable run trace, and the event buffer a turn's steps
//! and cost are folded from afterwards.
//!
//! Extracted from `CompanyAgent::run_with_steer` when turns moved onto
//! [`openhuman_embed::Agent::turn`] (plan hive-desks, Phase 2), whose
//! [`Turn::on_progress`](openhuman_embed::Turn::on_progress) takes the
//! sender half of an `mpsc` channel. The receiving half is what this module
//! is: an always-draining collector task, so the turn loop never blocks on a
//! full channel, that
//!
//! * re-labels each event with the curated step labels captured from the
//!   built tool set ([`StepLabels`]),
//! * publishes the console's ephemeral `tool_call` / `tool_result` /
//!   `thinking` frames through [`crate::turn_stream`] as they happen,
//! * writes each step through to the [`RunTraceSink`] when the turn is a
//!   dispatched card's, so the trace is durable *during* the run (issue #242),
//! * and buffers every event so the caller can fold
//!   [`steps`](super::steps) and read the turn's cost off
//!   `TurnCostUpdated` once the turn settles.
//!
//! Cost is read from the progress stream because the embed facade has no
//! `last_turn_usage`: `TurnCostUpdated` is cumulative for the turn and
//! `ModelCallCompleted` is per call, and the last cumulative figure is the
//! turn's total. When the model route is the loopback bridge
//! ([`crate::harness::model_bridge`]) the bridge's own usage tap is the
//! authoritative figure and this is the fallback.

use std::sync::Arc;

use openhuman_core as oh;

use oh::agent::progress::AgentProgress;

use super::run_trace::RunTraceSink;
use super::steps::{self, StepLabels};
use crate::harness::cost::TurnUsage;
use crate::turn_stream::{LiveRoute, TurnStreamCtx};

/// One turn's progress pump: hand [`sender`](Self::sender) to the turn, then
/// [`finish`](Self::finish) it to get every event back.
pub struct ProgressPump {
    tx: Option<tokio::sync::mpsc::Sender<AgentProgress>>,
    collector: tokio::task::JoinHandle<Vec<AgentProgress>>,
}

impl std::fmt::Debug for ProgressPump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressPump").finish_non_exhaustive()
    }
}

impl ProgressPump {
    /// Starts the collector for one turn.
    ///
    /// `stream` is the live route the console frames go out on (`None` for a
    /// turn nobody is watching); `run_sink` is the durable trace of a
    /// dispatched card (`None` for chat turns and workflow nodes).
    pub fn start(
        step_labels: StepLabels,
        stream: Option<TurnStreamCtx>,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentProgress>(1024);
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            let mut seq: u64 = 0;
            let mut thinking_open = false;
            while let Some(event) = rx.recv().await {
                let event = step_labels.apply(event);
                if let Some(ctx) = &stream
                    && let Some(frame) = steps::stream_event_from(&event, seq, &mut thinking_open)
                {
                    let frame = frame.with_agent(ctx.agent_id.clone());
                    let frame = match &ctx.route {
                        LiveRoute::Chat { chat_id } => frame.with_chat(chat_id.clone()),
                        LiveRoute::Workflow { run_id, node_id } => {
                            frame.with_workflow(run_id.clone(), node_id.clone())
                        }
                    };
                    let frame = frame.with_message_seq(ctx.message_seq);
                    crate::turn_stream::publish(&ctx.company, frame);
                    seq += 1;
                }
                if let Some(sink) = &run_sink {
                    sink.record(&event).await;
                }
                events.push(event);
            }
            if let Some(sink) = &run_sink {
                sink.flush().await;
            }
            events
        });
        Self {
            tx: Some(tx),
            collector,
        }
    }

    /// The sender the turn publishes into. Clone it per attempt: both attempts
    /// of a retried turn share one collector.
    pub fn sender(&self) -> tokio::sync::mpsc::Sender<AgentProgress> {
        self.tx
            .clone()
            .expect("the pump's sender is present until `finish`")
    }

    /// Closes the channel and returns every event the turn published, in
    /// order.
    pub async fn finish(mut self) -> Vec<AgentProgress> {
        drop(self.tx.take());
        self.collector.await.unwrap_or_default()
    }
}

/// The last cumulative cost figure the turn published, if any.
pub fn last_observed_turn_cost(events: &[AgentProgress]) -> Option<TurnUsage> {
    events.iter().rev().find_map(|event| match event {
        AgentProgress::TurnCostUpdated {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            total_usd,
            ..
        } => Some(TurnUsage {
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            cached_input_tokens: *cached_input_tokens,
            cost_usd: *total_usd,
        }),
        _ => None,
    })
}

/// Splits one collector's events into per-attempt segments, one per
/// `TurnStarted`, so a retried turn's second attempt is metered from its own
/// events rather than the first attempt's.
pub fn attempt_event_segments(events: &[AgentProgress], attempts: usize) -> Vec<&[AgentProgress]> {
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(i, event)| matches!(event, AgentProgress::TurnStarted).then_some(i))
        .collect();
    (0..attempts)
        .map(|i| match starts.get(i) {
            Some(&start) => {
                let end = starts.get(i + 1).copied().unwrap_or(events.len());
                &events[start..end]
            }
            None => &events[0..0],
        })
        .collect()
}

/// Whether the (last attempt of the) turn ran into its tool-iteration cap.
///
/// The embed facade has no `last_turn_hit_cap`; the cap is visible on the
/// stream instead — `IterationStarted` names both the iteration and the cap,
/// so a turn whose last iteration *is* the cap paused there rather than
/// finishing (issue #926). The `TurnCompleted` iteration count is preferred
/// when present, since it is what the loop itself reports.
/// Whether a turn should *report* the iteration cap, given that it may also
/// have been halted for spend.
///
/// #988 pins that a spend halt reads `hit_iteration_cap == false`, and
/// `brain.rs` depends on it: it emits the step-pause notice and the spend
/// notice from separate `if`s, on the stated grounds that the two "cannot both
/// come from ONE turn". While [`hit_iteration_cap`] never fired at all — it
/// required a `TurnStarted` no real stream emits — that invariant held for
/// free. Now that the predicate works, it has to be stated.
///
/// The halt wins because it is the more specific account of why the turn
/// stopped, and the notices are not interchangeable: a step pause invites
/// "continue", which on a spent budget would invite the operator to burn a cap
/// that has already run out.
#[must_use]
pub fn reportable_iteration_cap(raw_cap: bool, halted_for_spend: bool) -> bool {
    raw_cap && !halted_for_spend
}

pub fn hit_iteration_cap(events: &[AgentProgress]) -> bool {
    // **The scan is bounded by the previous turn's `TurnCompleted`, not by
    // `TurnStarted`.**
    //
    // This used to require a `TurnStarted` and return `false` without one —
    // and a real turn's progress stream does not carry one: it opens on
    // `IterationStarted`. So the cap was never reported, however many
    // iterations a turn burned. The same trap is written up one seam over,
    // where `attempt_event_segments` splits on the same never-emitted event.
    //
    // Dropping the precondition alone is not enough, and getting that wrong
    // is what made two `spend_halt_turn_tests` fail under CI's parallelism:
    // the `break` on `TurnStarted` was also the only thing confining the scan
    // to one turn. Without it the reverse walk ran the whole accumulated
    // buffer, so an earlier turn's `TurnCompleted { iterations: 25 }` counted
    // toward this turn and a turn that finished cleanly reported a pause it
    // never took.
    //
    // The boundary has to be an event the loop actually emits, so it is the
    // previous turn's `TurnCompleted`. The final event is skipped while
    // looking for it: a turn that ran to completion ends with its own
    // `TurnCompleted`, and that one closes *this* turn rather than opening it.
    let start = events
        .iter()
        .enumerate()
        .rev()
        .skip(1)
        .find(|(_, event)| matches!(event, AgentProgress::TurnCompleted { .. }))
        .map_or(0, |(index, _)| index + 1);

    let mut cap: Option<u32> = None;
    let mut iterations: u32 = 0;
    for event in events[start..].iter().rev() {
        match event {
            // Kept for a synthetic stream that does carry it; a real one
            // never reaches this arm.
            AgentProgress::TurnStarted => break,
            AgentProgress::TurnCompleted { iterations: n } => iterations = iterations.max(*n),
            AgentProgress::IterationStarted {
                iteration,
                max_iterations,
                ..
            } => {
                cap = cap.or(Some(*max_iterations));
                iterations = iterations.max(*iteration);
            }
            _ => {}
        }
    }
    matches!(cap, Some(cap) if cap > 0 && iterations >= cap)
}

#[cfg(test)]
#[path = "progress_pump_tests.rs"]
mod tests;
