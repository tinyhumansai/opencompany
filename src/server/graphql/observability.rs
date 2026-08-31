//! The run-observability read: what a company's agents actually did.
//!
//! Answers the question the REST run routes cannot: *given a workflow run, what
//! did each of its agent nodes do, step by step?* That join did not exist until
//! a workflow `agent` node started minting an attempt row — before it, a node's
//! turn had neither a card nor a conversation, so `RunStore` could not name it.
//!
//! # Why this is GraphQL and the timeline is REST
//!
//! `GET {scope}/runs/{id}` stays exactly as it is: it is shipping, tested, and
//! its shape is deliberately the console's `TimelineEntry` contract. This
//! surface exists for the *joined* read — run → attempts → steps → detail in one
//! request — which over REST would be one round trip per node and a client-side
//! assembly of the result.
//!
//! # The deep half
//!
//! [`RunStepGql::deep`] is unredacted: raw tool arguments, raw output, model
//! reasoning. It resolves through the same company scope every other field here
//! does, and it is `None` for a host that keeps no deep trace and for any step
//! that produced none. See [`crate::ports::deep_trace`] for what that store
//! holds and why it is separate.

use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::{Context, ID, Object, SimpleObject};

use crate::company::runtime::CompanyRuntime;
use crate::ports::deep_trace::TurnStepDetail;
use crate::ports::runs::{RunFilter, RunRecord, RunStepRecord};
use crate::server::approval_visibility;
use crate::server::graphql::auth::GqlAuth;

/// Token and cost totals for one attempt.
#[derive(SimpleObject, Default)]
#[graphql(name = "RunUsage")]
pub struct RunUsageGql {
    /// Input tokens.
    pub input_tokens: f64,
    /// Output tokens.
    pub output_tokens: f64,
    /// Input tokens served from the provider's cache.
    pub cached_input_tokens: f64,
    /// Cost in USD.
    pub cost_usd: f64,
}

/// The unredacted companion of one step. **Carries secrets by construction.**
#[derive(SimpleObject)]
#[graphql(name = "DeepStepDetail")]
pub struct DeepStepDetailGql {
    /// Model reasoning for a thinking step.
    pub reasoning: Option<String>,
    /// The tool's arguments as the model emitted them, unredacted.
    pub arguments: Option<String>,
    /// The tool's raw output, before it was reduced to a shape.
    pub output: Option<String>,
    /// The harness's own contextual label.
    pub display_detail: Option<String>,
    /// Which pass of the tool loop this step belongs to.
    pub iteration: Option<i32>,
    /// Whether the store clipped any field above to its cap.
    pub clipped: bool,
}

impl From<TurnStepDetail> for DeepStepDetailGql {
    fn from(d: TurnStepDetail) -> Self {
        Self {
            reasoning: d.reasoning,
            arguments: d.arguments,
            output: d.output,
            display_detail: d.display_detail,
            iteration: d.iteration.map(|i| i as i32),
            clipped: d.clipped,
        }
    }
}

/// One step of an attempt's trace.
#[derive(SimpleObject)]
#[graphql(name = "RunStep")]
pub struct RunStepGql {
    /// The step's ordinal within its run.
    pub seq: i32,
    /// When it was recorded.
    pub at_millis: f64,
    /// `tool_call` | `thinking` | `note`.
    pub kind: String,
    /// `ok` | `error` | `running` | `awaiting_approval`.
    pub status: String,
    /// The display label — the tool's name, or "Thinking".
    pub label: String,
    /// Arguments, **through the host redactor**. Safe to render anywhere.
    pub detail: Option<String>,
    /// A summary or shape of the result — never a remote body.
    pub result: Option<String>,
    /// The typed failure class, when the step failed.
    pub failure: Option<String>,
    /// Whether the harness truncated the result before we saw it.
    pub truncated: bool,
    /// Wall-clock duration.
    pub elapsed_ms: Option<f64>,
    /// The unredacted half. `None` when this host keeps no deep trace, and when
    /// the step produced none.
    pub deep: Option<DeepStepDetailGql>,
}

/// One attempt at work — a card dispatch, a chat turn, or a workflow node.
pub struct AgentRunGql {
    record: RunRecord,
    steps: Vec<RunStepRecord>,
    details: HashMap<u32, TurnStepDetail>,
}

#[Object(name = "AgentRun")]
impl AgentRunGql {
    /// The attempt id.
    async fn id(&self) -> ID {
        ID(self.record.id.clone())
    }

    /// The teammate that ran it.
    async fn agent_id(&self) -> String {
        self.record.agent_id.clone()
    }

    /// 1-based attempt ordinal at its card.
    async fn attempt(&self) -> i32 {
        self.record.attempt as i32
    }

    /// `pending` | `running` | `waiting_approval` | `paused` | `succeeded` |
    /// `failed` | `cancelled`.
    async fn status(&self) -> String {
        self.record.status.as_str().to_string()
    }

    /// `active` | `parked` | `terminal` — read this rather than inferring a
    /// phase from timestamps.
    async fn phase(&self) -> String {
        self.record.status.phase().to_string()
    }

    /// The card this attempted, when it attempted one.
    async fn task_id(&self) -> Option<ID> {
        self.record.task_id.clone().map(ID)
    }

    /// The conversation it belongs to, when one raised it.
    async fn chat_id(&self) -> Option<ID> {
        self.record.chat_id.clone().map(ID)
    }

    /// The workflow run whose node spawned it.
    async fn workflow_run_id(&self) -> Option<ID> {
        self.record.workflow_run_id.clone().map(ID)
    }

    /// The graph node within that run.
    async fn node_id(&self) -> Option<ID> {
        self.record.node_id.clone().map(ID)
    }

    /// When the row was opened.
    async fn created_at_millis(&self) -> f64 {
        self.record.created_at_millis as f64
    }

    /// When it began running.
    async fn started_at_millis(&self) -> Option<f64> {
        self.record.started_at_millis.map(|v| v as f64)
    }

    /// When it settled. `None` while it is still going.
    async fn finished_at_millis(&self) -> Option<f64> {
        self.record.finished_at_millis.map(|v| v as f64)
    }

    /// Why it failed, when it did.
    async fn error(&self) -> Option<String> {
        self.record.error.clone()
    }

    /// Token and cost totals. Provisional until the attempt settles — they are
    /// written by the settle, not accumulated on the row.
    async fn usage(&self) -> RunUsageGql {
        RunUsageGql {
            input_tokens: self.record.usage.input as f64,
            output_tokens: self.record.usage.output as f64,
            cached_input_tokens: self.record.usage.cached_input as f64,
            cost_usd: self.record.usage.cost_usd,
        }
    }

    /// The settled step count.
    ///
    /// **Null while the attempt is live**, deliberately: `step_count` is written
    /// by the settle, so returning the stored `0` for a running attempt would be
    /// a lie that a client cannot detect. A live reader counts `steps` instead,
    /// and this being `null` is what tells it to.
    async fn step_count(&self) -> Option<i32> {
        self.record
            .status
            .is_terminal()
            .then_some(self.record.step_count as i32)
    }

    /// The step trace, oldest first.
    async fn steps(&self) -> Vec<RunStepGql> {
        self.steps
            .iter()
            .map(|record| {
                let step = &record.step;
                RunStepGql {
                    seq: record.step_seq as i32,
                    at_millis: record.at_millis as f64,
                    kind: step.kind.wire_word().to_string(),
                    status: match step.status {
                        crate::ports::types::TurnStepStatus::Ok => "ok",
                        crate::ports::types::TurnStepStatus::Error => "error",
                        crate::ports::types::TurnStepStatus::Running => "running",
                        crate::ports::types::TurnStepStatus::AwaitingApproval => {
                            "awaiting_approval"
                        }
                    }
                    .to_string(),
                    label: step.label.clone(),
                    detail: step.detail.clone(),
                    result: step.result.clone(),
                    failure: step.failure.map(|f| f.wire_word().to_string()),
                    truncated: step.truncated,
                    elapsed_ms: step.elapsed_ms.map(|v| v as f64),
                    deep: self
                        .details
                        .get(&record.step_seq)
                        .cloned()
                        .map(DeepStepDetailGql::from),
                }
            })
            .collect()
    }
}

/// Loads one attempt with its trace and, when the host keeps one, its deep half.
///
/// The scrubbed skeleton is the primary answer, so a failure to read it must
/// reach the client rather than masquerade as "no steps".
///
/// `may_read_deep` is the principal's [`may_read_deep_trace`] verdict AND the
/// query's selection: the deep half carries unredacted secrets, so a caller who
/// may not read them — or who did not select `steps.deep` — is not even given a
/// store read that would be discarded. `None` for such a caller is
/// indistinguishable from "no deep trace recorded", which is the honest answer
/// to a reader who is not entitled to one.
///
/// [`may_read_deep_trace`]: crate::server::approval_visibility::may_read_deep_trace
async fn load(
    runtime: &Arc<CompanyRuntime>,
    record: RunRecord,
    may_read_deep: bool,
) -> async_graphql::Result<AgentRunGql> {
    let steps = runtime
        .runs()
        .list_run_steps(runtime.id(), &record.id)
        .await?;
    // A missing deep store, or a read that fails, degrades to "no deep half"
    // rather than failing the query: the scrubbed trace is the answer, and the
    // unredacted companion is the bonus.
    let details = if may_read_deep {
        runtime
            .deep_trace()
            .list_step_details(runtime.id(), &record.id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(assemble(record, steps, details))
}

/// Builds the GraphQL attempt from its skeleton, its steps and its deep half.
fn assemble(
    record: RunRecord,
    steps: Vec<RunStepRecord>,
    details: Vec<crate::ports::deep_trace::RunStepDetailRecord>,
) -> AgentRunGql {
    AgentRunGql {
        record,
        steps,
        details: details
            .into_iter()
            .map(|d| (d.step_seq, d.detail))
            .collect(),
    }
}

/// `Company.agentRuns` — attempts, newest first, optionally narrowed.
pub(crate) async fn resolve_runs(
    ctx: &Context<'_>,
    runtime: &Arc<CompanyRuntime>,
    task_id: Option<String>,
    workflow_run_id: Option<String>,
    limit: i32,
) -> async_graphql::Result<Vec<AgentRunGql>> {
    let auth = ctx.data::<GqlAuth>()?;
    let may_read_deep = approval_visibility::may_read_deep_trace(auth);
    // The deep half is read only when the selection actually asks for it. The
    // console's list query polls on 4/30-second intervals and deliberately
    // selects no `deep` bodies, so materializing up to `limit` runs × hundreds
    // of detail rows for a read nothing uses would be gigabytes of store I/O
    // and memory per poll — the lookahead is what keeps that from happening.
    let wants_deep = ctx.look_ahead().field("steps").field("deep").exists();
    let filter = RunFilter {
        task_id,
        workflow_run_id,
        agent_id: None,
        statuses: Vec::new(),
        limit: Some(limit.clamp(1, 200) as usize),
    };
    let rows = runtime.runs().list_runs(runtime.id(), &filter).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    // The index reads every returned run's trace, so fetch them all in one pass
    // rather than one store round trip per run. The filesystem backend rescans
    // the whole company-wide JSONL per per-run read, so a sequential loop would
    // be quadratic in company history on the view operators poll.
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let steps_by_run = runtime
        .runs()
        .list_run_steps_for_runs(runtime.id(), &ids)
        .await?;
    // The deep half degrades to "none" per run on any store failure, exactly as
    // the single-run read does. A caller who may not read the unredacted bodies
    // is not given the store read at all, and a query that does not select
    // `steps.deep` is not given it either — see `wants_deep` above.
    let details_by_run = if may_read_deep && wants_deep {
        runtime
            .deep_trace()
            .list_step_details_for_runs(runtime.id(), &ids)
            .await
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    Ok(rows
        .into_iter()
        .map(|record| {
            let id = record.id.clone();
            assemble(
                record,
                steps_by_run.get(&id).cloned().unwrap_or_default(),
                details_by_run.get(&id).cloned().unwrap_or_default(),
            )
        })
        .collect())
}

/// `Company.agentRun` — one attempt by id, or null.
pub(crate) async fn resolve_run(
    ctx: &Context<'_>,
    runtime: &Arc<CompanyRuntime>,
    id: String,
) -> async_graphql::Result<Option<AgentRunGql>> {
    let auth = ctx.data::<GqlAuth>()?;
    let may_read_deep = approval_visibility::may_read_deep_trace(auth);
    // The single-run read is the deep read, so it is almost always selected —
    // but the same lookahead guard keeps a `steps`-only query from dragging the
    // deep store into the request at all.
    let wants_deep = ctx.look_ahead().field("steps").field("deep").exists();
    let Some(record) = runtime.runs().get_run(runtime.id(), &id).await? else {
        return Ok(None);
    };
    Ok(Some(
        load(runtime, record, may_read_deep && wants_deep).await?,
    ))
}
