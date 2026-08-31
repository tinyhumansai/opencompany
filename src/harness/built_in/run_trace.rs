//! [`RunTraceSink`]: the durable, *incremental* trace of one task attempt.
//!
//! A dispatched card's turn already produces everything a trace needs — the
//! harness progress stream is drained live by the collector task in
//! [`CompanyAgent::run_with_steer`](crate::harness::CompanyAgent::run_with_steer),
//! which is where the console's live tool timeline is teed from. What was
//! missing was somewhere durable to put it: the folded
//! [`TurnStep`](crate::ports::types::TurnStep) list was handed back at the end
//! of the turn and then discarded into the card's note, so killing the host
//! mid-run left no evidence that any of it had happened.
//!
//! This sink hangs off that same seam and writes each step to the
//! [`RunStore`] **as it happens** (issue #242). Kill the process mid-run and
//! every step written so far survives — which is the property the issue exists
//! to create, and the opposite of a batch-at-the-end trace that loses the whole
//! run precisely when it is most interesting.
//!
//! ## Where the awaits happen
//!
//! Inside the collector task, never the model loop. The collector exists
//! specifically so a burst of progress events cannot block the turn; adding a
//! store write there means a slow store slows trace persistence and nothing
//! else. The write amplification — one row per step, versus one event per turn
//! before — is the explicit price of the feature, and it is affordable because
//! cycles serialise per company.
//!
//! ## Nothing here may fail the turn
//!
//! Every write is best-effort and logged. The tokens are already spent and the
//! agent's work is already done by the time a step is recorded; a full disk must
//! not turn a completed turn into a failed one. Same invariant as the inference
//! meter and the grant-consumption journal.
//!
//! Compiled only under `feature = "openhuman"`.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use openhuman_core::openhuman as oh;

use oh::agent::progress::AgentProgress;

use crate::harness::cost::TurnUsage;
use crate::harness::steps::StepTrace;
use crate::ports::RunStore;
use crate::ports::deep_trace::TurnStepDetail;
use crate::ports::now_millis;
use crate::ports::runs::RunStepRecord;
use crate::ports::types::{CompanyId, TokenUsage, TurnStep};

/// The most steps one attempt persists.
///
/// A bound, not a design constraint: a runaway tool loop must not write an
/// unbounded number of rows for a single card. Deliberately far above the
/// chat-timeline cap (`steps::MAX_STEPS`, 50) — that one exists to keep a
/// bubble readable, this one only to keep a pathological run from filling a
/// store — so a normal long-running attempt keeps its trace in full.
pub const MAX_RUN_STEPS: u32 = 500;

/// Collects one attempt's step trace and cost, writing steps through as they
/// happen.
///
/// **Run-scoped, not turn-scoped.** One sink spans every turn of the attempt —
/// the redirect re-runs and any delegate's turn — so ordinals stay dense across
/// the run and a later turn cannot overwrite an earlier one's rows (the store
/// keys steps on `(run_id, step_seq)`).
pub struct RunTraceSink {
    company: CompanyId,
    run_id: String,
    runs: Arc<dyn RunStore>,
    /// The incremental projection. Behind a `std` mutex because it is touched
    /// from the collector task and never held across an await.
    trace: StdMutex<StepTrace>,
    /// Tokens/cost folded across the attempt's turns.
    usage: StdMutex<TokenUsage>,
    /// How many step rows were actually written.
    persisted: StdMutex<u32>,
    /// Accumulated reasoning by step ordinal. `StepTrace` emits only the newly
    /// accumulated chunk after each threshold flush; the store record must still
    /// contain the complete prefix because each write replaces the prior row.
    deep_reasoning: StdMutex<std::collections::HashMap<u32, String>>,
    /// Where the unredacted companion of each step goes, when this host keeps
    /// one.
    ///
    /// **Presence of the `Arc` IS the enablement.** There is deliberately no
    /// boolean beside it: a host that does not retain deep traces constructs no
    /// store, so there is nothing to write to and nothing to get wrong. A future
    /// refactor replacing this with a flag would turn "cannot leak" into
    /// "must remember not to".
    deep: Option<Arc<dyn crate::ports::deep_trace::DeepTraceStore>>,
}

impl RunTraceSink {
    /// Opens a sink for `run_id` in `company`.
    pub fn new(company: CompanyId, run_id: impl Into<String>, runs: Arc<dyn RunStore>) -> Self {
        Self {
            company,
            run_id: run_id.into(),
            runs,
            trace: StdMutex::new(StepTrace::default()),
            usage: StdMutex::new(TokenUsage::default()),
            persisted: StdMutex::new(0),
            deep_reasoning: StdMutex::new(std::collections::HashMap::new()),
            deep: None,
        }
    }

    /// Also retain the unredacted companion of every step.
    ///
    /// Switches the trace itself into deep mode, so the two projections come
    /// from one state machine and their ordinals cannot drift.
    #[must_use]
    pub fn with_deep(
        mut self,
        deep: Option<Arc<dyn crate::ports::deep_trace::DeepTraceStore>>,
    ) -> Self {
        if deep.is_some() {
            self.trace = StdMutex::new(StepTrace::deep());
        }
        self.deep = deep;
        self
    }

    /// The attempt this sink is tracing.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Records one live progress event, persisting the step it maps to (if any).
    ///
    /// A start writes a `Running` row; its completion re-writes the *same*
    /// ordinal finalized, because `append_run_step` replaces on a matching
    /// `step_seq`. An event that maps to no step is a no-op.
    pub async fn record(&self, event: &AgentProgress) {
        let emitted = self
            .trace
            .lock()
            .expect("run trace")
            // The lock is released before the awaits below — a store write must
            // never be held across the mutex the collector re-enters per event.
            .push(event);
        self.persist(emitted).await;
    }

    /// Flushes a thinking run the stream never got to close.
    ///
    /// [`record`](Self::record) closes an open thought only when the next event
    /// gives it a reason to — visible text or a tool call. A turn that *ends*
    /// mid-thought — a reply, an abort, an error — has neither, so the tail of
    /// reasoning below the interim flush threshold would sit in the trace
    /// unpersisted. The collector calls this after the stream drains, so a
    /// failed or interrupted turn still keeps the reasoning that led to its end.
    /// No-op when nothing is open.
    pub async fn flush(&self) {
        let emitted = self.trace.lock().expect("run trace").finish();
        self.persist(emitted).await;
    }

    /// Writes one batch of emitted steps to the run store and — when this host
    /// retains deep traces — their unredacted companions.
    async fn persist(&self, emitted: Vec<(u32, TurnStep, Option<TurnStepDetail>)>) {
        for (step_seq, step, detail) in emitted {
            if step_seq >= MAX_RUN_STEPS {
                continue;
            }
            let at_millis = now_millis();
            let record = RunStepRecord {
                run_id: self.run_id.clone(),
                step_seq,
                at_millis,
                step,
            };
            let skeleton_written = match self.runs.append_run_step(&self.company, &record).await {
                Ok(()) => {
                    let mut persisted = self.persisted.lock().expect("run trace count");
                    // A finalized start rewrites its own row rather than adding
                    // one, so the count is the high-water ordinal, not the write
                    // count.
                    *persisted = (*persisted).max(step_seq + 1);
                    true
                }
                Err(err) => {
                    tracing::warn!(
                        company = %self.company,
                        run = %self.run_id,
                        step_seq,
                        error = %err,
                        "[runs] could not persist a step of an attempt's trace; the turn continues"
                    );
                    false
                }
            };
            if !skeleton_written {
                continue;
            }
            // never meet a detail whose step does not exist. Best-effort like
            // the step above: a full disk must degrade the record, never fail
            // the turn.
            let (Some(deep), Some(mut detail)) = (self.deep.as_ref(), detail) else {
                continue;
            };
            if detail.is_empty() {
                continue;
            }
            if let Some(reasoning) = detail.reasoning.take() {
                let mut buffers = self.deep_reasoning.lock().expect("deep reasoning");
                let buffer = buffers.entry(step_seq).or_default();
                buffer.push_str(&reasoning);
                // Each emitted chunk already passed through `bound_detail`, but
                // the concatenation can still exceed DEEP_REASONING_CHAR_CAP,
                // and every store trusts the caller's bound. Re-bind the
                // aggregate so a long reasoning stream cannot grow a row past
                // the documented 64 KiB bound with `clipped == false`.
                let mut bounded = crate::ports::deep_trace::bound_detail(TurnStepDetail {
                    reasoning: Some(buffer.clone()),
                    ..TurnStepDetail::default()
                });
                detail.reasoning = bounded.reasoning.take();
                detail.clipped |= bounded.clipped;
                // Cap the accumulator itself: only the first CAP bytes are ever
                // written, so keeping more in memory serves nothing.
                if let Some(bounded) = &detail.reasoning {
                    *buffer = bounded.clone();
                }
            }
            let record = crate::ports::deep_trace::RunStepDetailRecord {
                run_id: self.run_id.clone(),
                step_seq,
                at_millis,
                detail,
            };
            if let Err(err) = deep.append_step_detail(&self.company, &record).await {
                tracing::warn!(
                    company = %self.company,
                    run = %self.run_id,
                    step_seq,
                    error = %err,
                    "[runs] could not persist a step's deep detail; the turn continues"
                );
            }
        }
    }

    /// Folds one turn's token/cost totals into the attempt's.
    ///
    /// Called per turn rather than once at the end so a redirect re-run and a
    /// delegate's turn both count — an attempt's cost is what the *attempt*
    /// spent, not what its last turn did.
    pub fn add_usage(&self, turn: &TurnUsage) {
        let mut usage = self.usage.lock().expect("run usage");
        usage.input = usage.input.saturating_add(turn.input_tokens);
        usage.output = usage.output.saturating_add(turn.output_tokens);
        usage.cached_input = usage.cached_input.saturating_add(turn.cached_input_tokens);
        usage.cost_usd += turn.cost_usd;
    }

    /// The attempt's folded token/cost totals.
    ///
    /// Tokens are recorded even when the cost is zero: the managed `/openai/v1`
    /// passthrough bills backend-side and echoes no USD, so gating on cost would
    /// make every managed attempt read as having consumed nothing.
    pub fn usage(&self) -> TokenUsage {
        *self.usage.lock().expect("run usage")
    }

    /// How many step rows the attempt has written.
    pub fn step_count(&self) -> u32 {
        *self.persisted.lock().expect("run trace count")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ports::runs::{NewRun, RunFilter};
    use crate::ports::types::{TurnStepKind, TurnStepStatus};
    use crate::store::FsOps;

    fn started(call_id: &str, tool: &str, label: Option<&str>) -> AgentProgress {
        AgentProgress::ToolCallStarted {
            call_id: call_id.to_string(),
            tool_name: tool.to_string(),
            arguments: serde_json::Value::Null,
            iteration: 1,
            display_label: label.map(str::to_string),
            display_detail: None,
        }
    }

    fn thinking(delta: &str) -> AgentProgress {
        AgentProgress::ThinkingDelta {
            delta: delta.to_string(),
            iteration: 1,
        }
    }

    fn completed(call_id: &str, tool: &str) -> AgentProgress {
        AgentProgress::ToolCallCompleted {
            call_id: call_id.to_string(),
            tool_name: tool.to_string(),
            success: true,
            output_chars: 2,
            output: "ok".to_string(),
            arguments: Some(serde_json::json!({ "server": "brave", "tool": "search" })),
            elapsed_ms: 42,
            iteration: 1,
            failure: None,
        }
    }

    /// Builds an fs-backed run store with one `Pending` run, and returns the
    /// sink over it.
    async fn sink(home: &std::path::Path) -> (Arc<dyn RunStore>, CompanyId, RunTraceSink) {
        let company = CompanyId::new("acme");
        let runs: Arc<dyn RunStore> = Arc::new(FsOps::new(home.to_path_buf()));
        let run = runs
            .create_run(&company, NewRun::for_task("run-1", "t-1", "ceo"))
            .await
            .expect("mint");
        let sink = RunTraceSink::new(company.clone(), run.id, Arc::clone(&runs));
        (runs, company, sink)
    }

    /// The property the whole feature exists for: steps are durable **during**
    /// the run, so a host killed mid-tool-call still leaves the prefix behind.
    #[tokio::test]
    async fn steps_land_while_the_turn_is_still_running() {
        let home = tempfile::Builder::new()
            .prefix("opencompany-run-trace-")
            .tempdir()
            .expect("tempdir");
        let (runs, company, sink) = sink(home.path()).await;

        sink.record(&started("c1", "mcp_call_tool", Some("Searching")))
            .await;

        // Read it back BEFORE the turn ends — nothing has been folded yet.
        let steps = runs
            .list_run_steps(&company, "run-1")
            .await
            .expect("list steps");
        assert_eq!(steps.len(), 1, "the step is durable mid-turn");
        assert_eq!(steps[0].step_seq, 0);
        assert_eq!(steps[0].step.status, TurnStepStatus::Running);
        assert_eq!(steps[0].step.label, "Searching");
        assert_eq!(sink.step_count(), 1);

        // …and the completion finalizes that same row rather than adding one.
        sink.record(&completed("c1", "mcp_call_tool")).await;
        let steps = runs
            .list_run_steps(&company, "run-1")
            .await
            .expect("list steps");
        assert_eq!(steps.len(), 1, "a finalized start must not duplicate");
        assert_eq!(steps[0].step.status, TurnStepStatus::Ok);
        assert_eq!(steps[0].step.detail.as_deref(), Some("brave · search"));
        assert_eq!(steps[0].step.kind, TurnStepKind::ToolCall);
        assert_eq!(sink.step_count(), 1);
    }

    /// The EOF path end-to-end: a thought whose stream closes without a
    /// `TextDelta` or tool call still lands in the deep store when the sink is
    /// flushed. The tail below the interim flush threshold is exactly what an
    /// aborted turn leaves behind, and it must not vanish.
    #[tokio::test]
    async fn flush_persists_an_aborted_thoughts_tail() {
        let home = tempfile::Builder::new()
            .prefix("opencompany-run-trace-deep-")
            .tempdir()
            .expect("tempdir");
        let company = CompanyId::new("acme");
        let runs: Arc<dyn RunStore> = Arc::new(FsOps::new(home.path().to_path_buf()));
        let run = runs
            .create_run(&company, NewRun::for_task("run-1", "t-1", "ceo"))
            .await
            .expect("mint");
        let deep: Arc<dyn crate::ports::deep_trace::DeepTraceStore> =
            Arc::new(FsOps::new(home.path().to_path_buf()));
        let sink = RunTraceSink::new(company.clone(), run.id, Arc::clone(&runs))
            .with_deep(Some(Arc::clone(&deep)));

        sink.record(&thinking("first ")).await;
        sink.record(&thinking("second")).await; // under DEEP_THINK_FLUSH_BYTES
        // No text, no tool call — the turn just ends.
        sink.flush().await;

        let details = deep
            .list_step_details(&company, "run-1")
            .await
            .expect("list step details");
        let reasoning: String = details
            .iter()
            .filter_map(|d| d.detail.reasoning.clone())
            .collect();
        assert_eq!(
            reasoning, "first second",
            "the tail of an aborted thought was dropped: {reasoning:?}"
        );
    }

    /// Cost folds across every turn of the attempt, and tokens are recorded even
    /// at zero USD (the managed passthrough bills off the wire).
    #[test]
    fn usage_folds_across_turns_including_token_only_ones() {
        // Never written to — this test only exercises the in-memory fold.
        let runs: Arc<dyn RunStore> = Arc::new(FsOps::new(std::path::PathBuf::from("unused")));
        let sink = RunTraceSink::new(CompanyId::new("acme"), "run-1", runs);
        assert_eq!(sink.usage(), TokenUsage::default());

        sink.add_usage(&TurnUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 5,
            cost_usd: 0.25,
        });
        sink.add_usage(&TurnUsage {
            input_tokens: 10,
            output_tokens: 2,
            cached_input_tokens: 0,
            cost_usd: 0.0,
        });

        let usage = sink.usage();
        assert_eq!(usage.input, 110);
        assert_eq!(usage.output, 22);
        assert_eq!(usage.cached_input, 5);
        assert_eq!(usage.cost_usd, 0.25);
    }

    /// A store that cannot take a step must not be able to fail the turn — the
    /// agent's work is already done by the time a step is recorded.
    #[tokio::test]
    async fn a_store_failure_never_reaches_the_turn() {
        use async_trait::async_trait;

        use crate::error::OpenCompanyError;
        use crate::ports::runs::{RunRecord, RunStatus};

        struct BrokenRuns;

        #[async_trait]
        impl RunStore for BrokenRuns {
            async fn create_run(
                &self,
                company: &CompanyId,
                spec: NewRun,
            ) -> crate::Result<RunRecord> {
                Ok(RunRecord {
                    id: spec.id,
                    company: company.clone(),
                    task_id: spec.task_id,
                    chat_id: spec.chat_id,
                    agent_id: spec.agent_id,
                    attempt: 1,
                    status: RunStatus::Pending,
                    trigger_event_seq: None,
                    created_at_millis: 0,
                    started_at_millis: None,
                    finished_at_millis: None,
                    error: None,
                    usage: TokenUsage::default(),
                    step_count: 0,
                    workflow_run_id: None,
                    node_id: None,
                })
            }
            async fn get_run(
                &self,
                _company: &CompanyId,
                _id: &str,
            ) -> crate::Result<Option<RunRecord>> {
                Ok(None)
            }
            async fn put_run(&self, _company: &CompanyId, _run: &RunRecord) -> crate::Result<()> {
                Ok(())
            }
            async fn list_runs(
                &self,
                _company: &CompanyId,
                _filter: &RunFilter,
            ) -> crate::Result<Vec<RunRecord>> {
                Ok(Vec::new())
            }
            async fn append_run_step(
                &self,
                _company: &CompanyId,
                _step: &RunStepRecord,
            ) -> crate::Result<()> {
                Err(OpenCompanyError::Store("disk on fire".to_string()))
            }
            async fn list_run_steps(
                &self,
                _company: &CompanyId,
                _run_id: &str,
            ) -> crate::Result<Vec<RunStepRecord>> {
                Ok(Vec::new())
            }
        }

        let sink = RunTraceSink::new(CompanyId::new("acme"), "run-1", Arc::new(BrokenRuns));
        // No panic, no propagation — and the count stays honest about what
        // actually landed.
        sink.record(&started("c1", "mcp_call_tool", None)).await;
        assert_eq!(sink.step_count(), 0);
    }

    /// A runaway tool loop is bounded: past the cap nothing more is written, and
    /// the reported count stops at the cap rather than claiming rows that do not
    /// exist.
    #[tokio::test]
    async fn a_runaway_turn_stops_writing_at_the_cap() {
        let home = tempfile::Builder::new()
            .prefix("opencompany-run-trace-cap-")
            .tempdir()
            .expect("tempdir");
        let (runs, company, sink) = sink(home.path()).await;

        for i in 0..(MAX_RUN_STEPS + 5) {
            sink.record(&started(&format!("c{i}"), "spawn_task", None))
                .await;
        }
        assert_eq!(sink.step_count(), MAX_RUN_STEPS);
        assert_eq!(
            runs.list_run_steps(&company, "run-1")
                .await
                .expect("list")
                .len() as u32,
            MAX_RUN_STEPS
        );
    }
}
