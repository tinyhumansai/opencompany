//! The unredacted companion of a turn's [`TurnStep`]s.
//!
//! A [`TurnStep`] is what an operator sees: a label, redacted arguments, and a
//! *shape* of what came back ("12 items"). That is the right disclosure for a
//! timeline and an approval card, and it is deliberately lossy — the raw
//! arguments and the raw output are dropped, and a `thinking` step keeps no text
//! at all.
//!
//! This port keeps the other half, so a run can be read back and understood
//! rather than merely audited. It is a **sibling** store, not a widening of
//! [`RunStore`](crate::ports::runs::RunStore), and that separation is load
//! bearing three ways:
//!
//! - `TurnStep`'s wire contract is untouched, so nothing that renders a timeline
//!   can start leaking secrets by accident.
//! - `GET {scope}/runs/{id}` physically cannot disclose this: that route never
//!   calls this port.
//! - It can be purged wholesale ([`DeepTraceStore::purge_deep_trace`]) without
//!   touching run history, which is what an operator needs after debugging with
//!   it turned on.
//!
//! # This stores secrets, by design
//!
//! Unredacted tool arguments include credentials passed on a command line, and
//! raw output includes the contents of any file an agent read. Two rules follow,
//! and both are enforced rather than documented: the read path is company-scoped
//! exactly as every other per-company port is, and
//! `runtime::approval_display::redact` stays **unchanged** on the approval path —
//! widening the trace must never widen the operator-facing cards.
//!
//! # Bounds
//!
//! Raw output is unbounded by nature: a `cat` of a large file, a full test log.
//! So the caps live here rather than in the caller, and the prune is part of the
//! write, for the same reason [`MAX_RUN_OUTPUTS_PER_COMPANY`] is: a company can
//! never accumulate an unbounded pile, and putting it on the port keeps it a
//! property of the store rather than of whichever caller remembered to trim.
//!
//! Clip, never refuse. A body too large to keep is worth keeping the head of;
//! failing the write would lose the whole step, and the step is the record.
//!
//! [`MAX_RUN_OUTPUTS_PER_COMPANY`]: crate::ports::run_output::MAX_RUN_OUTPUTS_PER_COMPANY

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ports::types::CompanyId;

/// Characters of reasoning text kept for one step.
pub const DEEP_REASONING_CHAR_CAP: usize = 64 * 1024;

/// Characters of raw tool output kept for one step.
pub const DEEP_OUTPUT_CHAR_CAP: usize = 64 * 1024;

/// Characters of serialized tool arguments kept for one step.
pub const DEEP_ARGUMENTS_CHAR_CAP: usize = 32 * 1024;

/// Steps per run that may carry a detail record.
///
/// Matches the run trace's own step ceiling, so the two truncate at the same
/// ordinal: a reader never sees a detail for a step whose skeleton was dropped.
pub const MAX_DEEP_STEPS_PER_RUN: u32 = 500;

/// Runs per company that keep their detail bodies.
///
/// A quarter of [`MAX_RUN_OUTPUTS_PER_COMPANY`](crate::ports::run_output::MAX_RUN_OUTPUTS_PER_COMPANY)
/// on purpose: a detail record is far larger than an output snapshot, and the
/// question it answers ("what did this run actually do?") is asked about recent
/// runs. The skeleton in `RunStore` is never pruned by this — only the bodies go.
pub const MAX_DEEP_RUNS_PER_COMPANY: usize = 50;

/// Appended to a value this port had to clip.
const ELLIPSIS: &str = "…[clipped]";

/// The unredacted companion of one [`TurnStep`](crate::ports::types::TurnStep).
///
/// Every field is optional because a step is one of several shapes: a `thinking`
/// step has reasoning and no tool, a tool step has arguments and output and no
/// reasoning. Absent means "this step had none", never "this was withheld".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStepDetail {
    /// Model reasoning text, concatenated across the coalesced run of deltas
    /// that produced one `thinking` step. `None` on a tool step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// The tool's arguments as the model emitted them — **not** through
    /// `approval_display::redact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    /// The tool's raw output, before it was reduced to a shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The harness's own contextual label, dropped by the scrubbed fold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_detail: Option<String>,
    /// Which pass of the tool loop this step belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u32>,
    /// Whether any field above was clipped by the caps in this module.
    ///
    /// Distinct from a step's own `truncated`, which says the *harness* cut the
    /// result before we saw it. Both can be true, and they mean different things
    /// to a reader deciding whether the missing half is recoverable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clipped: bool,
}

impl TurnStepDetail {
    /// Whether this carries nothing worth storing.
    ///
    /// A step with no reasoning, no arguments and no output produced no detail —
    /// writing a row for it would cost a store round-trip per step to record
    /// that there was nothing to record.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reasoning.is_none()
            && self.arguments.is_none()
            && self.output.is_none()
            && self.display_detail.is_none()
    }
}

/// One [`TurnStepDetail`], addressed the way a
/// [`RunStepRecord`](crate::ports::runs::RunStepRecord) is.
///
/// `step_seq` is the **same** run-scoped ordinal the skeleton step carries, which
/// is what lets a reader join the two without a second key. Writing the same
/// `(run_id, step_seq)` twice replaces, so a step that is finalized in place —
/// a reasoning run that flushes partway and again at close — converges rather
/// than duplicating.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStepDetailRecord {
    /// The attempt this step belongs to.
    pub run_id: String,
    /// The step's ordinal within that run.
    pub step_seq: u32,
    /// When it was recorded.
    pub at_millis: u64,
    /// The unredacted half.
    pub detail: TurnStepDetail,
}

/// Clips every field of `detail` to this module's caps, flagging `clipped`.
///
/// Clip rather than refuse: see the module docs. Clipping is on a **character**
/// boundary, so a multi-byte sequence is never split into invalid UTF-8.
#[must_use]
pub fn bound_detail(mut detail: TurnStepDetail) -> TurnStepDetail {
    let mut clipped = detail.clipped;
    clip(&mut detail.reasoning, DEEP_REASONING_CHAR_CAP, &mut clipped);
    clip(&mut detail.output, DEEP_OUTPUT_CHAR_CAP, &mut clipped);
    clip(&mut detail.arguments, DEEP_ARGUMENTS_CHAR_CAP, &mut clipped);
    // `display_detail` is a one-line label from the harness; it rides the
    // argument cap because nothing else bounds it and a hostile one should not
    // be able to grow a row on its own.
    clip(
        &mut detail.display_detail,
        DEEP_ARGUMENTS_CHAR_CAP,
        &mut clipped,
    );
    detail.clipped = clipped;
    detail
}

/// Clips one optional field in place.
fn clip(field: &mut Option<String>, cap: usize, clipped: &mut bool) {
    let Some(text) = field.as_mut() else {
        return;
    };
    if text.chars().count() <= cap {
        return;
    }
    let end = text
        .char_indices()
        .nth(cap)
        .map_or(text.len(), |(index, _)| index);
    text.truncate(end);
    text.push_str(ELLIPSIS);
    *clipped = true;
}

/// Durable, per-company, unredacted step detail. Company A's trace MUST be
/// invisible to company B, exactly like every other per-company port.
#[async_trait]
pub trait DeepTraceStore: Send + Sync {
    /// Records (or replaces) one step's detail and prunes the company to its
    /// newest [`MAX_DEEP_RUNS_PER_COMPANY`] runs.
    ///
    /// The prune is part of the write for the same reason it is on
    /// [`WorkflowRunOutputStore`](crate::ports::run_output::WorkflowRunOutputStore):
    /// it keeps the cap a property of the port rather than of a caller.
    /// Re-writing the same `(run_id, step_seq)` is idempotent.
    ///
    /// Callers are expected to have passed the record through [`bound_detail`];
    /// an implementation may assume the caps but must not rely on it for
    /// correctness.
    async fn append_step_detail(
        &self,
        company: &CompanyId,
        record: &RunStepDetailRecord,
    ) -> Result<()>;

    /// Every detail recorded for one run, ordered by `step_seq`.
    ///
    /// An empty vector is the honest answer for a run that predates this
    /// feature, one whose bodies were pruned, and one that genuinely did nothing
    /// worth recording — a reader distinguishes them by whether the run's
    /// skeleton steps exist.
    async fn list_step_details(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Vec<RunStepDetailRecord>>;

    /// Every detail for every run in one call, keyed by run id.
    ///
    /// The provided implementation loops the per-run read. The filesystem
    /// backend overrides this for the same reason [`RunStore`]'s bulk steps
    /// read does: a per-run call rescans the whole company-wide JSONL, so the
    /// Observatory index would pay that scan once per listed run. See
    /// [`RunStore::list_run_steps_for_runs`](crate::ports::runs::RunStore::list_run_steps_for_runs).
    async fn list_step_details_for_runs(
        &self,
        company: &CompanyId,
        run_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<RunStepDetailRecord>>> {
        let mut out = std::collections::HashMap::with_capacity(run_ids.len());
        for id in run_ids {
            out.insert(id.clone(), self.list_step_details(company, id).await?);
        }
        Ok(out)
    }

    /// Destroys detail bodies: one run's when `run_id` is `Some`, the whole
    /// company's when `None`. Returns how many records went.
    ///
    /// Not optional garnish. This store holds secrets by design, so an operator
    /// who turned deep tracing on for one debugging session needs a single verb
    /// that destroys them, and a tenant migration needs one call that guarantees
    /// none travel. Leaves every `RunStepRecord` intact — the skeleton is the
    /// contract, the body is the luxury.
    async fn purge_deep_trace(&self, company: &CompanyId, run_id: Option<&str>) -> Result<u64>;
}

#[cfg(test)]
mod test {
    use super::*;

    fn detail(reasoning: Option<&str>, output: Option<&str>) -> TurnStepDetail {
        TurnStepDetail {
            reasoning: reasoning.map(str::to_string),
            output: output.map(str::to_string),
            ..TurnStepDetail::default()
        }
    }

    #[test]
    fn a_record_round_trips_as_camel_case_json() {
        let record = RunStepDetailRecord {
            run_id: "run-1".to_string(),
            step_seq: 3,
            at_millis: 42,
            detail: TurnStepDetail {
                reasoning: Some("memoise the chain".to_string()),
                arguments: Some(r#"{"command":"python3 solve.py"}"#.to_string()),
                output: Some("837799\n".to_string()),
                display_detail: Some("solve.py".to_string()),
                iteration: Some(2),
                clipped: false,
            },
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""runId":"run-1""#), "{json}");
        assert!(json.contains(r#""stepSeq":3"#), "{json}");
        assert!(json.contains(r#""displayDetail":"solve.py""#), "{json}");
        // `clipped: false` is skipped, so a record that was not clipped
        // serializes exactly as it did before the field existed.
        assert!(!json.contains("clipped"), "{json}");
        let back: RunStepDetailRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn an_absent_half_stays_absent() {
        // A tool step has no reasoning and a thinking step has no output; both
        // must serialize without the other's keys rather than as explicit nulls.
        let json = serde_json::to_string(&detail(Some("why"), None)).unwrap();
        assert!(json.contains("reasoning"));
        assert!(!json.contains("output"), "{json}");
    }

    #[test]
    fn a_step_with_nothing_to_say_is_empty() {
        assert!(TurnStepDetail::default().is_empty());
        assert!(!detail(Some("x"), None).is_empty());
        assert!(!detail(None, Some("x")).is_empty());
    }

    #[test]
    fn iteration_alone_does_not_make_a_record_worth_writing() {
        // Otherwise every step would write a row saying which loop pass it was
        // and nothing else.
        let only_iteration = TurnStepDetail {
            iteration: Some(4),
            ..TurnStepDetail::default()
        };
        assert!(only_iteration.is_empty());
    }

    #[test]
    fn a_value_under_the_cap_is_untouched() {
        let bounded = bound_detail(detail(Some("short"), Some("also short")));
        assert_eq!(bounded.reasoning.as_deref(), Some("short"));
        assert_eq!(bounded.output.as_deref(), Some("also short"));
        assert!(!bounded.clipped);
    }

    #[test]
    fn an_oversized_value_is_clipped_and_flagged() {
        let huge = "x".repeat(DEEP_OUTPUT_CHAR_CAP + 500);
        let bounded = bound_detail(detail(None, Some(&huge)));
        assert!(bounded.clipped);
        let kept = bounded.output.unwrap();
        assert!(kept.ends_with(ELLIPSIS));
        assert_eq!(
            kept.chars().count(),
            DEEP_OUTPUT_CHAR_CAP + ELLIPSIS.chars().count()
        );
    }

    #[test]
    fn clipping_never_splits_a_character() {
        // A multi-byte body clipped mid-sequence would be invalid UTF-8, which
        // `String::truncate` panics on rather than silently corrupting.
        let multibyte = "é".repeat(DEEP_OUTPUT_CHAR_CAP + 100);
        let bounded = bound_detail(detail(None, Some(&multibyte)));
        let kept = bounded.output.unwrap();
        assert!(bounded.clipped);
        assert!(kept.starts_with('é'));
        // Round-tripping proves it is still valid UTF-8.
        assert_eq!(String::from_utf8(kept.clone().into_bytes()).unwrap(), kept);
    }

    #[test]
    fn exactly_at_the_cap_is_not_clipped() {
        let exact = "x".repeat(DEEP_REASONING_CHAR_CAP);
        let bounded = bound_detail(detail(Some(&exact), None));
        assert!(!bounded.clipped);
        assert_eq!(
            bounded.reasoning.unwrap().chars().count(),
            DEEP_REASONING_CHAR_CAP
        );
    }

    #[test]
    fn each_field_has_its_own_cap() {
        // Arguments are capped tighter than output; a body that fits the output
        // cap must still be clipped when it arrives as arguments.
        let between = "x".repeat(DEEP_ARGUMENTS_CHAR_CAP + 10);
        let bounded = bound_detail(TurnStepDetail {
            arguments: Some(between.clone()),
            output: Some(between),
            ..TurnStepDetail::default()
        });
        assert!(bounded.arguments.unwrap().ends_with(ELLIPSIS));
        assert!(!bounded.output.unwrap().ends_with(ELLIPSIS));
    }

    #[test]
    fn an_already_clipped_record_stays_clipped() {
        // The flag is sticky: a caller that clipped upstream must not have it
        // cleared by a second pass that happened to find everything in bounds.
        let bounded = bound_detail(TurnStepDetail {
            reasoning: Some("short".to_string()),
            clipped: true,
            ..TurnStepDetail::default()
        });
        assert!(bounded.clipped);
    }
}
