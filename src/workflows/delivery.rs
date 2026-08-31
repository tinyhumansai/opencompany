//! Route an `output` node's report to a person or a channel (issue #170).
//!
//! An `output` node is a workflow's terminal "report back". Before this module
//! existed it produced a value that surfaced only in the console's transient
//! run-result drawer — so a workflow could compute an owner summary and had no
//! way to send it anywhere. A node's
//! [`destination`](crate::company::WorkflowDestinationDef) closes that gap.
//!
//! `destination` is **optional**, and a node without one is the pre-#170 shape
//! rather than a deliberate "route nothing": every graph authored before it
//! existed still has one, including all 21 seeded company templates. Such a node
//! is not delivered from, but it does now produce a `Skipped` /
//! [`NoDestinationConfigured`](crate::ports::DeliveryReason::NoDestinationConfigured)
//! row (issue #925) so that an unconfigured destination is distinguishable from
//! a run that genuinely had nothing to route. Silence made those two identical.
//!
//! # Where this runs, and why here
//!
//! Delivery is **host-side and post-engine**: [`deliver_outputs`] is called from
//! [`run_workflow_inner`](super::runner) once `tinyflows::engine::run` has
//! returned, NOT from the HTTP handler. Three callers drive the same
//! [`WorkflowRunner`](crate::ports::WorkflowRunner) port — the console's run
//! route, the orchestrator's `run_workflow` tool, and the trigger scheduler —
//! and a *scheduled* run is precisely the case where nobody is watching the
//! drawer. Putting delivery in the handler would give the console the only
//! working destination.
//!
//! The engine never sees a destination. That is why it is a first-class model
//! field rather than node `config`: config is lowered into the engine graph, and
//! an inert key riding into the engine is exactly what the reserved-key
//! validation exists to prevent.
//!
//! # The security boundary
//!
//! **No path here may let a workflow email an arbitrary external address without
//! an explicit company grant.** The three destination kinds are gated
//! differently because they carry different risk:
//!
//! * **`owner`** — recipients are resolved *server-side* from the company's own
//!   [`UserStore`](crate::ports::UserStore) (active `Admin` users). The graph
//!   names no address, so an author cannot point it at an outsider. Constrained
//!   by construction; no grant needed. With no admin address (or no mailbox) it
//!   falls back to the **durable** operator channel (issue #1757) — the report
//!   is journaled into the operator's main line, a real, readable delivery —
//!   rather than dead-ending on the interactive in-memory buffer as it once did.
//! * **`email`** — the graph names an arbitrary address, so it is the dangerous
//!   one and carries **two independent gates**, both fail-closed:
//!   1. the company's `[tools].allow` must cover the `email` namespace (the same
//!      [`grants_cover`](crate::harness::build::grants_cover) matcher that gates
//!      an agent's `email.send` effect and a workflow `tool_call`), and
//!   2. the recipient must be an **established thread** — the company's inbox
//!      must already hold inbound mail from that address. This is the rule
//!      ported verbatim from the agent send path
//!      ([`crate::runtime::cycle`]); a cold recipient is **parked for operator
//!      approval**, never sent to on the workflow's own authority.
//! * **`channel`** — the target must match a [`ChannelAdapter`] the deployment
//!   already wired. A graph cannot conjure a channel; it can only address one an
//!   operator installed. Constrained by construction, like `owner`.
//!
//! # A cold recipient is delayed, not dropped (issue #227)
//!
//! A cold `email` recipient used to end the report's life: a `skipped` row and
//! nothing else, while a teammate emailing the same new contact got an approval
//! card. Delivery now parks the send the same way the agent path does, so the
//! two paths refuse identically and both refusals are recoverable.
//!
//! **This does not resume the run, and does not need to.** Delivery is
//! post-engine by design: by the time a destination is refused,
//! `tinyflows::engine::run` has already returned and the run is complete —
//! there is nothing to resume. What is parked is the *send*, not the run, and
//! approving it executes through the same
//! [`perform_effect`](crate::runtime::cycle) path that mails an agent's
//! approved `email.send` from an HTTP handler outside any live cycle.
//!
//! One consequence to be honest about: a workflow run is not persisted, so a
//! `pending` row is a **snapshot** taken at delivery time and can never later
//! flip to `sent`. The approvals queue is the live source of truth — it is
//! journal-backed and survives a restart; the row only points at it.
//!
//! The park is **direct**, never routed through
//! [`ApprovalGate::evaluate`](crate::ports::ApprovalGate): under `full` policy
//! mode that returns `Allow` for a `Send` effect, which would auto-send cold
//! workflow mail on most companies and turn the established-thread gate into a
//! suggestion. See `park_cold_recipient` below.
//!
//! # A report is delivered once per approval lineage (issue #438)
//!
//! Resuming a workflow gate is a **re-run**: the engine settles when it pauses,
//! so continuing walks the graph again from the trigger and every `output` node
//! upstream of the gate is reached a second time. Delivery is state-based — an
//! established recipient stays established — so approving a gate used to mail
//! the same report to the same person again, as a direct result of clicking
//! Approve.
//!
//! [`deliver_outputs`] therefore takes a **delivery ledger**: the `{node, kind}`
//! rows this lineage has already sent or parked, threaded onto the
//! continuation's trigger input by the approval that started it (see
//! [`crate::runtime::workflow_resume`]). A reached node on that list is skipped
//! with [`DeliveryReason::AlreadyDelivered`] and nothing is dispatched.
//!
//! A `Pending` row counts as delivered, which closes a second hole in the same
//! place: [`park_cold_recipient`] has no dedupe of its own, so a continuation
//! used to stack a second identical cold-send card — and approving both would
//! send the mail twice.
//!
//! # Failure is reported, never fatal
//!
//! A delivery failure must not fail a run that already did its work. Every
//! attempt yields a [`DeliveryReport`] row on
//! [`WorkflowRun::deliveries`](crate::ports::WorkflowRun). On an **on-demand**
//! run those rows ride the run response into the console's run-result panel, so
//! an operator can tell a delivered report from an undelivered one without
//! reading a log. A **scheduled** run is not persisted, so its rows reach only
//! the scheduler's log until issue #228 surfaces them. There is one attempt per
//! recipient and no retry: a workflow run is not a mail queue.
//!
//! An output node the run never reached (an untaken branch, or a path that
//! paused for approval) gets no attempt and no row — an absent row means "not
//! reached", never "silently dropped".
//!
//! # Two reasons per row, and why (issue #248)
//!
//! Every row carries both a free-text [`DeliveryReport::detail`] and a
//! [`DeliveryReason`]. They are not redundant — they have different readers:
//!
//! * `detail` is for the **operator**: the run response and the
//!   `WorkflowRunFinished` history their own console reads back. It may quote a
//!   transport verbatim, which is what makes a failed send diagnosable.
//! * `reason` is for the **host log**. The scheduler's undelivered-report
//!   warning goes to host stdout, which on a hosted deployment is the platform
//!   and not the operator — the same boundary
//!   [`crate::runtime::workflow_scheduler`]'s module docs draw. A transport
//!   refusal quotes the mailbox it refused (`550 5.1.1
//!   <recipient@example.invalid>: Recipient address rejected`), so `detail` is
//!   not loggable there and `reason` is.
//!
//! Both are set at the same construction site, so the classification is made
//! where the outcome is known rather than recovered later by pattern-matching a
//! string. `DeliveryReason` has no `String` payload, so the safe half cannot
//! drift into carrying transport text without changing its type.

use std::sync::Arc;

use serde_json::Value;

use crate::company::WorkflowFile;
use crate::company::runtime::CompanyMail;
use crate::company::{WorkflowDestinationDef, WorkflowNodeKind};
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, CompanyRecord, Effect, EffectGroup,
    OutboundMessage, Verdict,
};
use crate::ports::{
    ApprovalGate, ChannelAdapter, DeliveryReason, DeliveryReport, DeliveryStatus, EmailRecord,
    EventLog, InboxStore, UserRole, UserStatus, UserStore, generate_id, normalize_email,
    now_millis,
};
use crate::runtime::cycle::EMAIL_SEND_KIND;
use crate::runtime::journal::{ApprovalConversation, RuntimeJournal};
use crate::runtime::workflow_resume::DeliveredReport;
use crate::server::ops::mailer::{MailCredentials, OutboundEmail};
use crate::server::ops::smtp::local_part;

/// How much report text one delivery carries. A workflow can emit an arbitrarily
/// large payload; an email or chat message that large helps nobody and may be
/// refused by the transport, so the body is truncated (on a **character**
/// boundary — never a byte slice, which panics mid-codepoint) with a visible
/// marker so the reader knows the text was cut.
const MAX_REPORT_CHARS: usize = 16_000;

/// The marker appended when a report is truncated at [`MAX_REPORT_CHARS`].
const TRUNCATION_MARKER: &str = "\n\n… (report truncated)";

/// The ports an output destination needs, bundled so
/// [`HarnessDeps`](crate::harness::HarnessDeps) grows one optional field rather
/// than four.
///
/// [`HarnessDeps::delivery`](crate::harness::HarnessDeps) is `Option<Self>` and
/// defaults to `None` at every construction site except the production runtime
/// builder. `None` **fails closed and loud**: [`deliver_outputs`] attempts
/// nothing and writes a `failed` row naming the gap, so an operator sees "this
/// build cannot deliver" in the run result instead of an authored destination
/// quietly doing nothing.
#[derive(Clone)]
pub struct WorkflowDeliveryDeps {
    /// The company's own outbound-mail handle (sender + its SMTP credentials).
    /// `None` when the company has no mailbox: `owner` then reports a failed
    /// delivery, and `email` is reported `skipped`.
    pub mail: Option<CompanyMail>,
    /// The company's inboxes — both the established-thread check and the
    /// outbound audit record go through this port.
    pub inbox: Arc<dyn InboxStore>,
    /// The company's user directory: how an `owner` destination resolves to
    /// actual addresses, server-side.
    pub users: Arc<dyn UserStore>,
    /// The deployment's standing bootstrap-admin address
    /// ([`AppConfig::bootstrap_admin`](crate::app::AppConfig::bootstrap_admin)),
    /// pre-normalized, when the platform injected one (issue #661 / M8).
    ///
    /// A platform-provisioned company has nobody in its manifest and nobody in
    /// the [`UserStore`](Self::users) until the creator first signs in, so on a
    /// fresh tenant an `owner` report used to find no admin address and fall
    /// back to the operator channel — the one human who could act on it never
    /// heard about it. This is the same standing invite the login path honours
    /// (`server::users::bootstrap_admins`), threaded here so `owner` reaches it
    /// before that first sign-in. `None` — the only value every non-production
    /// construction site sets — is a clean no-op.
    ///
    /// The `Debug` impl prints its presence only, never the address, the same
    /// stance the mail handle takes.
    pub bootstrap_admin: Option<String>,
    /// Wired delivery adapters. The interactive `operator` adapter is never
    /// present here — `RuntimeBuilder::build` drops it by identity and
    /// substitutes a durable, journal-backed
    /// [`DurableOperatorChannel`](crate::runtime::channel::DurableOperatorChannel)
    /// under the same id, so `operator` is a first-class delivery target
    /// (`post_to_operator`, `send_to_channel_adapter`), not a rejected one
    /// (issue #1757).
    pub channels: Vec<Arc<dyn ChannelAdapter>>,
    /// What a cold `email` recipient is parked on (issue #227). `None` fails
    /// closed to the pre-#227 behaviour: the report is `skipped`, never a
    /// `pending` row no queue is backing.
    pub parking: Option<DeliveryParking>,
    /// The company's event journal, for the write-behind delivery record
    /// (issue #529). Every dispatch that actually leaves the process — a `Sent`
    /// send, or a `Pending` park whose card sends on approval — appends one
    /// [`CompanyEvent::WorkflowReportDelivered`], so a run that crashes before
    /// its [`WorkflowRunFinished`](CompanyEvent::WorkflowRunFinished) is written
    /// still leaves a durable ledger a re-run can consult and skip.
    ///
    /// Non-optional and the same handle the runner reads for its progress trail,
    /// unlike [`parking`](Self::parking): the write-behind is the whole point of
    /// this field, and the only construction site is the production builder,
    /// where the journal always exists. The append is best-effort — a failure
    /// warns and is swallowed, never failing a delivery whose work already
    /// happened, the same stance as
    /// [`record_run_finished`](crate::runtime::record_run_finished).
    pub events: Arc<dyn EventLog>,
}

/// The approval queue's two halves, bundled so a delivery can only ever hold
/// **both or neither**.
///
/// Deliberately one field rather than two `Option`s. Parking on the gate
/// without journaling would produce an approval that is invisible to
/// `/approvals` (which reads the journal, not the gate) and gone on the next
/// restart — a card the operator can neither see nor approve, backing a
/// `pending` row that promises one exists. Making that state unrepresentable is
/// cheaper than remembering not to build it.
#[derive(Clone)]
pub struct DeliveryParking {
    /// Where the effect is parked, yielding the
    /// [`ApprovalId`](crate::ports::types::ApprovalId) the operator later
    /// resolves.
    pub approvals: Arc<dyn ApprovalGate>,
    /// The durable record of the park. This is what `/approvals` lists and what
    /// boot replay rehydrates, so it is what makes the card survive a restart.
    pub journal: Arc<RuntimeJournal>,
    /// How many decisions each turn is still blocked on (issue #469), so a park
    /// raised **outside** a cycle can join a batch too.
    ///
    /// Added by issue #978. Before it, this path armed nothing and passed no
    /// turn key, so every gate of a fan-out was its own batch of one: each
    /// believed it was the last decision outstanding and each re-dispatched the
    /// whole run. The same handle the runtime resolves against — a second queue
    /// would count parks nobody releases.
    pub continuations: crate::runtime::continuation::ContinuationQueue,
    /// Which gate node each parked workflow approval is deciding, and the
    /// trigger input its run paused with (issue #978).
    ///
    /// Armed in lockstep with [`continuations`](Self::continuations) and for the
    /// same reason they are one struct rather than two options: a run whose
    /// decisions are counted but whose gates are not recorded releases a batch
    /// the host cannot re-dispatch.
    pub gates: crate::runtime::workflow_gates::WorkflowGateQueue,
    /// The workflow id and trigger input each blocked agent node needs to
    /// re-dispatch its run (issue #899, Stage 1).
    ///
    /// Armed by the runner at block-settle (not here, and not in
    /// [`park_and_journal`](DeliveryParking::park_and_journal) — the parker has
    /// no trigger input), and released by the runtime's `continue_turn`. The same
    /// handle both sides share, for [`gates`](Self::gates)' reason.
    pub blocked_nodes: crate::runtime::blocked_nodes::BlockedNodeQueue,
}

impl std::fmt::Debug for WorkflowDeliveryDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the mail handle itself — `CompanyMail` carries `SmtpCredentials`,
        // whose derived `Debug` prints the password (see `mailer::test`).
        f.debug_struct("WorkflowDeliveryDeps")
            .field("mail", &self.mail.is_some())
            // Presence only: a bootstrap-admin address is a real person's email
            // and must never reach a log line, exactly like the mail handle.
            .field("bootstrap_admin", &self.bootstrap_admin.is_some())
            .field("parking", &self.parking.is_some())
            .field(
                "channels",
                &self
                    .channels
                    .iter()
                    .map(|c| c.channel_id().to_string())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// Delivers every reached `output` node's report to its configured destination,
/// returning one [`DeliveryReport`] per attempt.
///
/// Never returns an error: a delivery problem is data on the run result, not a
/// failed run (see the module docs). Nodes with no `destination`, and nodes the
/// run never reached, produce no rows at all.
///
/// `already_delivered` is this lineage's delivery ledger (issue #438): the
/// reports a run *earlier in the same approval chain* already sent or parked. A
/// reached node listed there is skipped with
/// [`DeliveryReason::AlreadyDelivered`] and **nothing is dispatched** — it is
/// empty on every run nobody resumed, so an ordinary run is untouched. See
/// [`crate::runtime::workflow_resume`] for why a continuation reaches these
/// nodes again at all. Issue #529 widens the same guard across a *crash*: the
/// caller unions this per-lineage ledger with a durable one folded from the
/// journal, so an independently re-run workflow skips what a crashed run already
/// sent.
///
/// `run_id` is the run this dispatch belongs to — it rides every
/// [`CompanyEvent::WorkflowReportDelivered`] this call appends (issue #529), so
/// the durable delivery record correlates with the run's start and per-node
/// trail exactly as [`WorkflowRunFinished`](CompanyEvent::WorkflowRunFinished)
/// does.
pub async fn deliver_outputs(
    delivery: Option<&WorkflowDeliveryDeps>,
    record: &CompanyRecord,
    workflow: &WorkflowFile,
    run_id: &str,
    output: &Value,
    already_delivered: &[DeliveredReport],
) -> Vec<DeliveryReport> {
    let mut reports = Vec::new();

    for node in &workflow.nodes {
        // Only `output` nodes report back. Validation already rejects a
        // `destination` on any other kind; this is the belt to that braces, so a
        // graph loaded from an older/looser source can never deliver from, say,
        // an agent node.
        if node.kind != WorkflowNodeKind::Output {
            continue;
        }
        // An output node the run never reached (untaken branch, or a path that
        // paused for approval) is not a delivery that failed — it is a delivery
        // that was never owed. No attempt, no row.
        //
        // Checked BEFORE the destination arm below, and that order is the whole
        // of issue #925: a node with no destination still has to be reached
        // before its absence is worth reporting. An unreached node contributes
        // nothing either way, exactly as before.
        if !node_was_reached(output, &node.id) {
            tracing::debug!(
                company = %record.id,
                workflow = %workflow.id,
                node = %node.id,
                "workflow delivery: output node not reached; nothing to deliver"
            );
            continue;
        }
        // Issue #925: the run reached a terminal report-back node that names
        // nowhere to report to. This used to be a bare `continue` — no row, no
        // log, nothing — which is why every run of a graph authored before
        // destinations existed ends `Finished — this run routed no reports.`
        // That sentence is true and useless: it reads identically whether the
        // author routed nothing deliberately or never configured a destination,
        // and the second is a fixable mistake nobody could see.
        let Some(destination) = &node.destination else {
            // An output node that exists to PAUSE is control flow, not a
            // report-back that lost its address: `requires_approval` makes the
            // engine stop on it, and a graph can use an `output` node for
            // nothing else (`DELIVER_THEN_GATE` in `workflows::runner` is
            // exactly that shape). Telling its author to "give the node a
            // destination" would be wrong advice on every run of a correct
            // workflow, and a row nobody should act on is how a row everybody
            // should act on gets ignored.
            //
            // The trade-off, stated plainly: an author who forgets a destination
            // on an approval-gated output node is not warned about that node.
            // Every ungated output node in the same graph still reports, and a
            // false alarm on every gated run is the worse of the two.
            if node.requires_approval.unwrap_or(false) {
                tracing::debug!(
                    company = %record.id,
                    workflow = %workflow.id,
                    node = %node.id,
                    "workflow delivery: approval-gated output node has no destination; \
                     treated as a gate, not a missing address"
                );
                continue;
            }
            tracing::info!(
                company = %record.id,
                workflow = %workflow.id,
                node = %node.id,
                "workflow delivery: output node has no destination; nothing was routed"
            );
            reports.push(DeliveryReport {
                node: node.id.clone(),
                // No destination was authored, so there is no kind to echo. The
                // literal reads correctly in the operator's row (`→ none — …`)
                // and keeps the field a plain token rather than an empty string
                // the console would render as a gap.
                kind: "none".to_string(),
                target: None,
                status: DeliveryStatus::Skipped,
                detail: "this output node has no destination, so its report was not sent \
                         anywhere — open the workflow and give the node a destination to \
                         deliver it"
                    .to_string(),
                reason: DeliveryReason::NoDestinationConfigured,
            });
            continue;
        };

        // Issue #438: a run earlier in this approval lineage already delivered
        // this node's report. The continuation reached the node again because
        // resuming re-runs the graph from the trigger, not because anything is
        // owed — so this is a skip with a reason, and **no dispatch of any
        // kind**. Checked before the unwired-ports arm below on purpose: a
        // report that already went out must not be reported as one this build
        // could not send.
        if already_delivered.iter().any(|prior| prior.node == node.id) {
            tracing::info!(
                company = %record.id,
                workflow = %workflow.id,
                node = %node.id,
                kind = %destination.kind,
                "workflow delivery: an earlier run in this approval lineage already delivered this \
                 report; not sending it again"
            );
            reports.push(DeliveryReport {
                node: node.id.clone(),
                kind: destination.kind.clone(),
                target: destination.target.clone(),
                status: DeliveryStatus::Skipped,
                detail: "this report was already delivered by an earlier run of this workflow — \
                         approving a gate re-runs the graph from the start, and a report that has \
                         already gone out is not sent a second time"
                    .to_string(),
                reason: DeliveryReason::AlreadyDelivered,
            });
            continue;
        }

        let text = report_text(output, &node.id);
        let subject = subject_for(record, workflow, &node.name);

        let Some(delivery) = delivery else {
            // The #169 lesson: a silent skip is indistinguishable from a working
            // destination. Say it where the operator actually looks — the run
            // result — and in the log.
            tracing::warn!(
                company = %record.id,
                workflow = %workflow.id,
                node = %node.id,
                kind = %destination.kind,
                "workflow delivery: this build has no delivery ports wired; the report was NOT sent"
            );
            reports.push(DeliveryReport {
                node: node.id.clone(),
                kind: destination.kind.clone(),
                target: destination.target.clone(),
                status: DeliveryStatus::Failed,
                detail: "report delivery is not wired on this runtime — the workflow ran and its \
                         result is in this run, but nothing was sent"
                    .to_string(),
                reason: DeliveryReason::NotWired,
            });
            continue;
        };

        // Issue #529: journal WRITE-BEHIND — after `deliver_one` has dispatched,
        // not before. Every row it just pushed for this node whose outcome
        // actually left the process (`Sent`, or a `Pending` park whose durable
        // card sends on approval) gets one `WorkflowReportDelivered` line, so a
        // crash before the run's finish still leaves a ledger a re-run can skip.
        // The skip/failed rows above never reach here, and none of them left the
        // process anyway.
        let before = reports.len();
        deliver_one(
            delivery,
            record,
            &node.id,
            destination,
            &subject,
            &text,
            &mut reports,
        )
        .await;
        for report in &reports[before..] {
            journal_delivered(&delivery.events, &record.id, &workflow.id, run_id, report).await;
        }
    }

    reports
}

/// The dry-run counterpart of [`deliver_outputs`] (issue #542): runs only the
/// **routing** half and stops before any dispatch.
///
/// For every reached `output` node that carries a destination it pushes one
/// `Skipped` / [`DeliveryReason::DryRun`] row naming where the report *would*
/// have gone; a reached node that names **no** destination pushes a `Skipped` /
/// [`DeliveryReason::NoDestinationConfigured`] row instead (issue #925), because
/// "nowhere" is the answer a test run most needs to give. Nothing leaves the
/// process: no transport, no cold-recipient park, no journal write. A node the
/// run never reached contributes no row, exactly as in the live path — so the
/// rows are an honest map of the reached output destinations, which is what a
/// test run exists to prove.
///
/// Takes no [`WorkflowDeliveryDeps`] and needs none: a dry run wires no delivery
/// ports (and no journal write is owed), so this is a pure function of the graph
/// and the run's output. It never journals a [`WorkflowReportDelivered`], so the
/// #529 ledger is left untouched — by two mechanisms, since a `Skipped` row
/// would not be journaled even on the live path.
pub fn deliver_outputs_dry(
    record: &CompanyRecord,
    workflow: &WorkflowFile,
    output: &Value,
) -> Vec<DeliveryReport> {
    let mut reports = Vec::new();
    for node in &workflow.nodes {
        if node.kind != WorkflowNodeKind::Output {
            continue;
        }
        // The routing half: an output node the run never reached is not a
        // delivery that was skipped, it is one that was never owed — no row, the
        // same rule the live path takes.
        if !node_was_reached(output, &node.id) {
            tracing::debug!(
                company = %record.id,
                workflow = %workflow.id,
                node = %node.id,
                "workflow dry delivery: output node not reached; nothing to route"
            );
            continue;
        }
        // Issue #925, same rule as the live path. A test run exists to answer
        // "where would this go?", and "nowhere, because the node names no
        // destination" is the answer an author most needs to see *before*
        // scheduling it.
        let Some(destination) = &node.destination else {
            // A gate is control flow, not a report — same rule as the live path.
            if node.requires_approval.unwrap_or(false) {
                continue;
            }
            tracing::info!(
                company = %record.id,
                workflow = %workflow.id,
                node = %node.id,
                "workflow dry delivery: output node has no destination; nothing would be routed"
            );
            reports.push(DeliveryReport {
                node: node.id.clone(),
                kind: "none".to_string(),
                target: None,
                status: DeliveryStatus::Skipped,
                detail: "this output node has no destination, so a real run would not send its \
                         report anywhere — give the node a destination to deliver it"
                    .to_string(),
                reason: DeliveryReason::NoDestinationConfigured,
            });
            continue;
        };
        let where_to = match destination
            .target
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(target) => format!("{} {}", destination.kind, target),
            None => destination.kind.clone(),
        };
        tracing::debug!(
            company = %record.id,
            workflow = %workflow.id,
            node = %node.id,
            kind = %destination.kind,
            "workflow dry delivery: report routed but NOT sent (test run)"
        );
        reports.push(DeliveryReport {
            node: node.id.clone(),
            kind: destination.kind.clone(),
            target: destination.target.clone(),
            status: DeliveryStatus::Skipped,
            detail: format!(
                "this was a test run — nothing was sent; the report would have gone to {where_to}"
            ),
            reason: DeliveryReason::DryRun,
        });
    }
    reports
}

/// Appends one [`CompanyEvent::WorkflowReportDelivered`] for a delivery row that
/// left the process (issue #529).
///
/// Only `Sent` and `Pending` are journaled, and the reasoning is the crash story
/// the whole event exists for. A `Sent` row's mail is out of the process; a
/// `Pending` row is a durable, journal-backed approval card that sends on
/// approval — both count as delivered, exactly as issue #438's ledger counts
/// them (see [`crate::runtime::workflow_resume`]). `Skipped` / `Denied` /
/// `Failed` journal nothing: nothing left the process, so a re-run is free to
/// retry them.
///
/// Best-effort, like every write on this path: a failure is logged loud and
/// swallowed. Losing the record risks one duplicate on a later re-run — the
/// accepted cost of write-behind — and is never worth failing a delivery whose
/// send already happened.
async fn journal_delivered(
    events: &Arc<dyn EventLog>,
    company: &CompanyId,
    workflow_id: &str,
    run_id: &str,
    report: &DeliveryReport,
) {
    if !matches!(
        report.status,
        DeliveryStatus::Sent | DeliveryStatus::Pending
    ) {
        return;
    }
    let event = CompanyEvent::WorkflowReportDelivered {
        workflow_id: workflow_id.to_string(),
        run_id: run_id.to_string(),
        node: report.node.clone(),
        kind: report.kind.clone(),
        target: report.target.clone(),
    };
    if let Err(err) = events.append(company, event).await {
        // Swallowed on purpose: the report already went out. Losing this record
        // means a re-run might send it a second time, which is the write-behind
        // trade-off — a loud line, never a failed delivery.
        tracing::warn!(
            %company,
            workflow = %workflow_id,
            %run_id,
            node = %report.node,
            %err,
            "workflow delivery: the delivered-report record could not be journaled; the report \
             itself was sent, but a later re-run may not know it already went out"
        );
    }
}

/// Dispatches one node's destination, appending every attempt's row to
/// `reports`. `owner` can fan out to several admins, so this appends rather than
/// returning a single report.
async fn deliver_one(
    delivery: &WorkflowDeliveryDeps,
    record: &CompanyRecord,
    node_id: &str,
    destination: &WorkflowDestinationDef,
    subject: &str,
    text: &str,
    reports: &mut Vec<DeliveryReport>,
) {
    // `reason` sits between `status` and `detail` on purpose: the classification
    // is not optional trailing garnish, and a caller has to walk past it to
    // reach the free-text half.
    let row = |target: Option<String>,
               status: DeliveryStatus,
               reason: DeliveryReason,
               detail: String| DeliveryReport {
        node: node_id.to_string(),
        kind: destination.kind.clone(),
        target,
        status,
        detail,
        reason,
    };
    let target = destination.target.as_deref().map(str::trim).unwrap_or("");

    match destination.kind.trim() {
        // --- owner: resolved server-side; the graph names nobody -------------
        "owner" => {
            let admins = owner_recipients(
                delivery.users.as_ref(),
                &record.id,
                record,
                delivery.bootstrap_admin.as_deref(),
            )
            .await;
            match (&delivery.mail, admins.is_empty()) {
                (Some(mail), false) => {
                    for address in admins {
                        let result =
                            send_email(delivery, mail, &record.id, &address, subject, text).await;
                        reports.push(match result {
                            Ok(()) => row(
                                Some(address),
                                DeliveryStatus::Sent,
                                DeliveryReason::OwnerEmailed,
                                "emailed the company's admin".to_string(),
                            ),
                            // `err` is the transport's own words and can quote
                            // the mailbox it refused, so it stays on the
                            // operator's half only (issue #248).
                            Err(err) => row(
                                Some(address),
                                DeliveryStatus::Failed,
                                DeliveryReason::MailTransportRefused,
                                format!("the mail transport refused the message: {err}"),
                            ),
                        });
                    }
                }
                // No mailbox, or no admin has an address: fall back to the
                // DURABLE operator channel (issue #1757). It journals the report
                // into the operator's main line, so this is a genuine, readable
                // delivery — not the discard-on-an-in-memory-buffer this arm used
                // to report. It is a `Sent` row, and the fallback only fails when
                // no operator adapter is wired at all (a misconfigured build),
                // never silence.
                _ => {
                    let (why, why_reason) = if delivery.mail.is_none() {
                        (
                            "no mailbox is configured for this company",
                            DeliveryReason::OwnerFellBackNoMailbox,
                        )
                    } else {
                        (
                            "no active admin or standing admin invite has an email address",
                            DeliveryReason::OwnerFellBackNoAdminAddress,
                        )
                    };
                    reports.push(
                        post_to_operator(delivery, record, subject, text)
                            .await
                            .map(|()| {
                                row(
                                    Some(crate::runtime::channel::OPERATOR_CHANNEL.to_string()),
                                    DeliveryStatus::Sent,
                                    why_reason,
                                    format!("{why}, so the report went to the operator channel"),
                                )
                            })
                            // The channel's own failure class (`_class`) is
                            // dropped in favour of naming the fallback, which is
                            // the part an operator reading a host log needs: the
                            // interesting fact is that `owner` had nowhere left to
                            // go. The full text, class included, is on `detail`.
                            .unwrap_or_else(|(_class, detail)| {
                                row(
                                    Some(crate::runtime::channel::OPERATOR_CHANNEL.to_string()),
                                    DeliveryStatus::Failed,
                                    DeliveryReason::OwnerFallbackFailed,
                                    format!(
                                        "{why}, and the operator channel fallback failed: {detail}"
                                    ),
                                )
                            }),
                    );
                }
            }
        }

        // --- email: the graph names an address, so it is double-gated --------
        "email" => {
            // GATE 1 — the company must grant the `email` namespace. Checked
            // FIRST and independently of whether mail is even wired, so a
            // missing grant is always reported as a denial rather than being
            // masked by an unrelated configuration gap.
            if !crate::harness::build::grants_cover(&record.manifest.tools.allow, "email") {
                tracing::warn!(
                    company = %record.id,
                    node = %node_id,
                    "workflow delivery: refused an email destination — the company does not grant `email`"
                );
                reports.push(row(
                    Some(target.to_string()),
                    DeliveryStatus::Denied,
                    DeliveryReason::EmailNotGranted,
                    "this company's [tools].allow does not grant `email`, so a workflow may not \
                     send mail to a named address"
                        .to_string(),
                ));
                return;
            }
            let Some(mail) = &delivery.mail else {
                reports.push(row(
                    Some(target.to_string()),
                    DeliveryStatus::Skipped,
                    DeliveryReason::NoMailboxConfigured,
                    "no mailbox is configured for this company, so there is nothing to send from"
                        .to_string(),
                ));
                return;
            };
            // GATE 2 — the established-thread rule, ported from the agent send
            // path. Fails closed: an inbox read error counts as "cold".
            if !recipient_is_established(
                delivery.inbox.as_ref(),
                &record.id,
                &mail.smtp.from_email,
                target,
            )
            .await
            {
                reports.push(
                    park_cold_recipient(delivery, record, node_id, target, subject, text, row)
                        .await,
                );
                return;
            }
            reports.push(
                match send_email(delivery, mail, &record.id, target, subject, text).await {
                    Ok(()) => row(
                        Some(target.to_string()),
                        DeliveryStatus::Sent,
                        DeliveryReason::RecipientEmailed,
                        "emailed the named recipient on an established thread".to_string(),
                    ),
                    // The `email` arm is where the leak mattered most: `target`
                    // IS the recipient's address, and an SMTP refusal echoes it
                    // back inside `err`. Classified here, so the log line can
                    // say what failed without saying to whom (issue #248).
                    Err(err) => row(
                        Some(target.to_string()),
                        DeliveryStatus::Failed,
                        DeliveryReason::MailTransportRefused,
                        format!("the mail transport refused the message: {err}"),
                    ),
                },
            );
        }

        // --- channel: only a channel the deployment already wired ------------
        "channel" => {
            reports.push(
                match post_to_channel(delivery, record, target, subject, text).await {
                    Ok(()) => row(
                        Some(target.to_string()),
                        DeliveryStatus::Sent,
                        DeliveryReason::ChannelPosted,
                        "posted to the channel".to_string(),
                    ),
                    Err((reason, detail)) => row(
                        Some(target.to_string()),
                        DeliveryStatus::Failed,
                        reason,
                        detail,
                    ),
                },
            );
        }

        // Unreachable through `parse_workflow`, which rejects an unknown kind.
        // Reported rather than ignored so a graph that somehow bypassed
        // validation cannot deliver nowhere in silence.
        other => reports.push(row(
            destination.target.clone(),
            DeliveryStatus::Failed,
            DeliveryReason::UnknownDestinationKind,
            format!("`{other}` is not a destination kind this runtime knows how to deliver to"),
        )),
    }
}

/// Parks a cold `email` recipient's report for operator approval, returning the
/// row that says so (issue #227).
///
/// # Why this parks DIRECTLY instead of asking the gate
///
/// The obvious shape — evaluate the effect, then act on the decision — is
/// wrong here, and dangerously so.
/// [`ManifestApprovalGate::evaluate`](crate::policy::ManifestApprovalGate)
/// returns `Allow` for a `Send` effect under `full` policy mode, and
/// `email.send` is not in the always-approve list. Routing through it would
/// therefore **auto-send** this mail on every full-mode company — which is most
/// of them — quietly converting the established-thread rule from a gate into a
/// suggestion. The refusal is already decided by the time we get here; the only
/// question is whether the report is dropped or recoverable. So this takes the
/// same already-decided path [`CycleHost::park_effect`](crate::ports::brain::CycleHost)
/// does on the agent side: park, journal, done.
///
/// **The invariant: a cold recipient never auto-sends.** With no parking wired
/// the report degrades to the pre-#227 `skipped` row; if parking itself errors
/// it degrades to `skipped` too. Nothing about a cold recipient reaches a
/// transport without a human verdict.
async fn park_cold_recipient(
    delivery: &WorkflowDeliveryDeps,
    record: &CompanyRecord,
    node_id: &str,
    target: &str,
    subject: &str,
    text: &str,
    row: impl Fn(Option<String>, DeliveryStatus, DeliveryReason, String) -> DeliveryReport,
) -> DeliveryReport {
    let Some(parking) = &delivery.parking else {
        // Fail closed to the pre-#227 behaviour. A `pending` row on a runtime
        // with no approvals queue would point the operator at a card that does
        // not exist.
        tracing::warn!(
            company = %record.id,
            node = %node_id,
            "workflow delivery: skipped an email destination — the recipient is not an established \
             thread and this runtime has no approvals queue to park it on"
        );
        return row(
            Some(target.to_string()),
            DeliveryStatus::Skipped,
            DeliveryReason::RecipientNotEstablished,
            "this recipient has never written to the company, so a workflow may not open the \
             conversation — send once from the inbox first"
                .to_string(),
        );
    };

    // The same effect shape the agent path builds in `CycleHostImpl::send_email`
    // — same kind, same group, same counterparty flags, same payload keys — so
    // the operator sees one kind of card and `perform_effect` executes it on
    // approval through the code that already ships.
    let effect = Effect {
        kind: EMAIL_SEND_KIND.into(),
        group: EffectGroup::Send,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: true,
        payload: serde_json::json!({
            "to": target,
            "subject": subject,
            // Already truncated by `report_text`.
            "body": text,
        }),
        agent: None,
        run_id: None,
    };

    // No board task is behind a workflow delivery, so the approval is parked
    // explicitly unlinked (issue #333) — recorded as "belongs to no card" rather
    // than left blank, so no task's Approvals tab adopts it by happening to be
    // mid-run when it parked. Issue #379: it has no conversation behind it
    // either, so there is no thread to raise it in; it stays Approvals-page-only.
    match parking
        .park_and_journal(
            &record.id,
            effect,
            crate::runtime::journal::TaskLink::Unlinked,
            None,
            // Issue #978: no turn. A cold-recipient card is one delivery's own
            // decision, not one of a run's batch — it resolves and sends on its
            // own, exactly as before.
            None,
        )
        .await
    {
        Ok(_) => {
            tracing::info!(
                company = %record.id,
                node = %node_id,
                "workflow delivery: parked an email destination for operator approval — the \
                 recipient is not an established thread"
            );
            row(
                Some(target.to_string()),
                DeliveryStatus::Pending,
                DeliveryReason::ParkedForApproval,
                "this recipient has never written to the company, so a workflow may not open the \
                 conversation on its own — the report is waiting for you in Approvals, and \
                 approving it sends the mail"
                    .to_string(),
            )
        }
        Err(err) => {
            // The queue is the only thing that failed; the refusal itself still
            // holds. Report the pre-#227 outcome and say why it was not parked.
            //
            // `err` is deliberately the only thing interpolated: this line goes
            // to host stdout, which on a hosted tenant is us and not the
            // operator, so the recipient's address must not ride it (issue
            // #248). The row the operator reads names the target; the log does
            // not.
            tracing::warn!(
                company = %record.id,
                node = %node_id,
                error = %err,
                "workflow delivery: could not park a cold email destination for approval; the \
                 report was NOT sent"
            );
            row(
                Some(target.to_string()),
                DeliveryStatus::Skipped,
                DeliveryReason::ParkingUnavailable,
                "this recipient has never written to the company, and this report could not be \
                 queued for your approval either — send once from the inbox first"
                    .to_string(),
            )
        }
    }
}

impl DeliveryParking {
    /// Parks `effect` on the gate and journals it — **both halves or neither**.
    ///
    /// The gate is in-memory; the journal is the durable record `/approvals`
    /// reads and boot replay rehydrates. A gate entry the journal never recorded
    /// is the worst of the three possible outcomes: it shows up in the
    /// operator's queue now, vanishes on the next restart, and backs a `pending`
    /// row that promises a card which no longer exists.
    ///
    /// Bundling the two handles in [`DeliveryParking`] makes the *mis-wiring* of
    /// that state unrepresentable, but it does nothing about a **partial failure
    /// at runtime** — `park` succeeding and `record_parked` erroring (a full
    /// disk, a read-only volume, a serialization fault). So the journal write is
    /// treated as the commit point: if it fails, the gate entry is retracted
    /// before returning the error, and the caller degrades to whatever it does
    /// when parking is unavailable.
    ///
    /// Retraction has to undo **two** things, because a failed `record_parked`
    /// has already mutated the journal's in-memory queue (it inserts before it
    /// appends, so the entry is live even though nothing reached disk):
    ///
    /// 1. [`ApprovalGate::resolve`] with [`Verdict::Deny`] — the trait's only
    ///    removal verb, and the honest one: this effect must never execute. It
    ///    is attributed to
    ///    [`ActorKind::System`](crate::ports::types::ActorKind::System) (the
    ///    runtime itself, as boot replay and the TTL sweep are) rather than to
    ///    an operator who made no such decision.
    /// 2. [`RuntimeJournal::record_resolved`] — which also removes before it
    ///    appends, so it clears the in-memory queue entry that would otherwise
    ///    show the operator a card `/approvals` lists but the gate can no longer
    ///    execute. Its own append will usually fail for the same reason the
    ///    first one did; that is fine and expected, since there is no
    ///    `ApprovalParked` line on disk for it to pair with anyway.
    ///
    /// The ordering cannot simply be inverted to dodge this: `record_parked`
    /// needs the [`ApprovalId`](crate::ports::types::ApprovalId) that `park`
    /// mints, so the gate write must come first.
    ///
    /// Both rollback steps deliberately ignore their own errors and the
    /// **original** journal error propagates — the effect is unparked either
    /// way, and losing the real cause behind a cleanup error would make the
    /// failure harder to diagnose, not easier.
    ///
    /// # Why this is `pub(crate)` rather than a private free function (#395)
    ///
    /// It was private to this module while cold-recipient delivery was the only
    /// caller. Issue #395 found two more places that must park an effect from
    /// *outside* a cycle — a workflow agent node's gated tool call, and a
    /// `requires_approval` node the engine paused on — and neither has a
    /// [`CycleHost`](crate::ports::brain::CycleHost) to reach
    /// [`park_effect`](crate::ports::brain::CycleHost::park_effect) through.
    ///
    /// Widening `CycleHost` for them would have been the wrong seam: that trait
    /// is the *cycle's* whole effect surface, and a workflow run is not a cycle.
    /// What all three callers actually share is this transaction. So it becomes
    /// a method on the bundle that already carries both handles — and which is
    /// already threaded down the workflow path as
    /// [`HarnessDeps::delivery`](crate::harness::HarnessDeps)`.parking`.
    ///
    /// `task_link` and `thread` are parameters rather than the hardcoded
    /// `Unlinked` / `None` delivery used, because they are the two facts only
    /// the caller knows: which board card owns the request, and which
    /// conversation to raise it in.
    pub(crate) async fn park_and_journal(
        &self,
        company: &CompanyId,
        effect: Effect,
        task_link: crate::runtime::journal::TaskLink,
        thread: Option<String>,
        turn: Option<String>,
    ) -> Result<ApprovalId, crate::error::OpenCompanyError> {
        // Issue #1825 (P1, fifth follow-up — found by chatgpt-codex-connector):
        // arm this card's continuation slot BEFORE anything below can make the
        // approval visible to a concurrent resolver. `record_parked`'s
        // synchronous in-memory insert — the write `approval_cycle` reads to
        // route a resolution through the continuation batch — lands as soon as
        // that call's synchronous portion runs, strictly before its own async
        // durable append (below) returns; a resolve racing in on another tokio
        // worker thread during that window used to see a turn whose only armed
        // slot was `park_gated_calls`'s pre-loop synthetic hold — this card's
        // own arm had not run yet, still gated behind the journal write below —
        // consumed it, and released the batch before this card (or the rest of
        // the node's batch) had finished parking. This card's own arm then
        // still ran once the journal write returned, into a queue entry the
        // premature decision had already removed: a fresh, orphaned slot no
        // further decision would ever redeem, doubling the eventual dispatch.
        // Arming here, before the approval gate has even minted an id, closes
        // the window by construction — nothing below can make this card
        // resolvable before its slot is already counted.
        if let Some(turn) = turn.as_deref() {
            self.continuations.arm(turn);
        }
        let approval_id = match self.approvals.park(company, effect.clone()).await {
            Ok(id) => id,
            Err(err) => {
                // Nothing was ever parked, so no decision will ever come along
                // to release the slot armed above — release it now instead of
                // leaving the turn blocked on a card that will never exist.
                if let Some(turn) = turn.as_deref() {
                    self.continuations.decide(turn, None);
                }
                return Err(err);
            }
        };
        if let Err(err) = self
            .journal
            .record_parked(
                &approval_id,
                &effect,
                now_millis(),
                task_link,
                // A channel but no thread root (issue #435), for a reason one
                // step upstream of #469's: a workflow node's request is not
                // raised by a chat message, so there is no message for a
                // continuation to hang under. The channel is the whole of the
                // conversation identity here, exactly as before.
                ApprovalConversation {
                    thread,
                    parent: None,
                },
                // The turn this park belongs to, when it belongs to one
                // (issues #469, #978).
                //
                // `None` for a cold-recipient delivery and for an agent node's
                // gated tool call: neither is raised by anything that holds a
                // continuation, so each resolves and continues on its own,
                // exactly as it always has.
                //
                // `Some` for a `requires_approval` gate, where issue #978 found
                // the opposite: the N gates one run pauses on ARE a batch, and
                // recording no key for them is what let every branch of a
                // fan-out believe it was the last decision and re-dispatch the
                // whole run. A run is a turn in precisely the sense #469 means —
                // one unit of work, blocked on several decisions, owed exactly
                // one continuation when the last of them lands.
                turn.clone(),
            )
            .await
        {
            // Roll back to "never parked". Both steps deliberately swallow their
            // own errors — `err` below is the one worth surfacing.
            if let Err(rollback) = self
                .approvals
                .resolve(
                    &approval_id,
                    Verdict::Deny,
                    Actor {
                        kind: ActorKind::System,
                        id: "workflow-delivery".to_string(),
                    },
                )
                .await
            {
                tracing::error!(
                    company = %company,
                    error = %rollback,
                    "workflow: a parked effect could not be journaled AND could not be \
                     retracted from the approval gate; it may linger in the queue until restart"
                );
            }
            // Clears the in-memory queue entry `record_parked` inserted before
            // it failed to write. Its append will usually fail too — expected,
            // and ignored: there is no `ApprovalParked` line on disk to pair
            // with.
            let _ = self.journal.record_resolved(&approval_id).await;
            // Same as the park failure above: the card this slot was armed for
            // was just retracted, so release it rather than leave the turn
            // blocked forever on a decision that can never arrive.
            if let Some(turn) = turn.as_deref() {
                self.continuations.decide(turn, None);
            }
            return Err(err);
        }
        // Issue #978: arm the gate queue once the park is durable. `gates` is
        // looked up by `approval_id` (minted above) rather than by turn alone,
        // so — unlike `continuations`, moved ahead of this function's first
        // await for the reason at the top — it has no visibility-before-count
        // window of its own to close: nothing can look this approval's gate up
        // before `approval_id` exists, which is true either way. `park_pending_gates`'
        // dedupe skip never reaches here at all.
        if let Some(turn) = turn {
            self.gates.arm(&turn, &approval_id, &effect);
        }
        Ok(approval_id)
    }
}

/// The addresses an `owner` report is emailed to: the company's active admins,
/// unioned with its **standing admin invites** — the manifest's `[users]
/// admins` and the deployment's [`bootstrap_admin`](WorkflowDeliveryDeps::bootstrap_admin)
/// (issue #661 / M8) — that have not yet signed in.
///
/// # Why the union, and why the "no user record" restriction
///
/// A platform-provisioned company names nobody in its manifest and has nobody
/// in the [`UserStore`] until the creator redeems their first login link. The
/// pre-M8 resolver read only the store, so on a fresh tenant an owner report
/// found no admin address and fell back to the operator channel — the one human
/// who could act on it never got it. The standing invites are exactly the
/// addresses the login path (`server::users::eligibility` /
/// [`bootstrap_admins`](crate::server::users)) already treats as admins-in-waiting,
/// so `owner` mails them for the same reason they can log in.
///
/// A **user record wins** over a standing invite for the same address, mirroring
/// `eligibility`: a standing invite is only mailed when the address holds *no*
/// record at all. Two consequences fall out of that one rule —
///
/// * a bootstrap admin who has since signed in **and been suspended** is not
///   mailed (their record wins, and a suspended admin is not an active one), and
/// * an address named both as an active admin and as a standing invite is
///   mailed **once** (the active-admin arm sends it; the standing copy is
///   dropped as "already has a record").
///
/// # Store-error stance: still mail the standing invites
///
/// An unreadable user store yields the standing invites **anyway**, not an empty
/// list. The store failing is precisely when dropping the only humans the
/// company is known to have is worst — that silent drop back to the operator
/// channel is the M8 bug. The read failure is logged; the standing invites,
/// which come from the manifest and the injected config and need no store read,
/// are still mailed. An empty result (no admins, no standing invites) routes
/// `owner` to the operator-channel fallback exactly as before.
async fn owner_recipients(
    users: &dyn UserStore,
    company: &CompanyId,
    record: &CompanyRecord,
    bootstrap_admin: Option<&str>,
) -> Vec<String> {
    // The standing admin invites: the manifest's `[users] admins` plus the
    // platform-injected bootstrap admin, normalized the same way the login path
    // normalizes them so `Grace@ACME.test` and `grace@acme.test` are one
    // address here and there. `bootstrap_admin` arrives already normalized (the
    // `AppConfig` accessor did it), but normalizing again is idempotent and
    // keeps this function honest against a caller that passes a raw value.
    let mut standing: Vec<String> = record
        .manifest
        .users
        .admins
        .iter()
        .map(|a| normalize_email(a))
        .collect();
    if let Some(email) = bootstrap_admin {
        let email = normalize_email(email);
        if !email.is_empty() && !standing.contains(&email) {
            standing.push(email);
        }
    }

    match users.list_users(company).await {
        Ok(list) => {
            // Every address that holds a record, whatever its role or status.
            // These win: a standing invite for such an address is dropped, so a
            // suspended admin is not resurrected through a leftover invite and a
            // double-listed address is mailed once.
            let has_record: std::collections::HashSet<String> =
                list.iter().map(|u| normalize_email(&u.email)).collect();
            // The send-eligible records: active admins with a real mailbox.
            let mut recipients: Vec<String> = list
                .iter()
                .filter(|u| u.role == UserRole::Admin && u.status == UserStatus::Active)
                .map(|u| u.email.clone())
                .filter(|email| email.contains('@'))
                .collect();
            // Standing invites with no record yet, and a real mailbox.
            for email in standing {
                if !has_record.contains(&email)
                    && email.contains('@')
                    && !recipients.contains(&email)
                {
                    recipients.push(email);
                }
            }
            recipients
        }
        Err(err) => {
            tracing::warn!(
                company = %company,
                error = %err,
                "workflow delivery: could not read the user directory; emailing the standing admin \
                 invites only (dropping them would silence the owner report entirely)"
            );
            // Still mail the standing invites — they are the only humans the
            // company is known to have, and this drop is the M8 bug.
            standing
                .into_iter()
                .filter(|email| email.contains('@'))
                .collect()
        }
    }
}

/// Sends one email through the company's own mail handle and mirrors it into the
/// company inbox as outbound (the same audit trail the agent send path and the
/// console's test-send leave, and what makes the thread "established" for a
/// later reply).
async fn send_email(
    delivery: &WorkflowDeliveryDeps,
    mail: &CompanyMail,
    company: &CompanyId,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), crate::error::OpenCompanyError> {
    let email = OutboundEmail {
        to: to.to_string(),
        subject: subject.to_string(),
        body: body.to_string(),
    };
    mail.sender
        .send(&MailCredentials::Smtp(mail.smtp.clone()), &email)
        .await?;
    record_outbound(delivery.inbox.as_ref(), company, mail, &email).await;
    Ok(())
}

/// Appends a sent email to the sending inbox so the console shows it alongside
/// inbound mail. Mirrors [`crate::server::ops::smtp::record_outbound`], which
/// takes a whole `CompanyRuntime` this path does not have.
async fn record_outbound(
    inbox: &dyn InboxStore,
    company: &CompanyId,
    mail: &CompanyMail,
    email: &OutboundEmail,
) {
    let record = EmailRecord {
        id: generate_id(),
        inbox: local_part(&mail.smtp.from_email),
        from_name: mail.smtp.from_name.clone(),
        from_email: mail.smtp.from_email.clone(),
        subject: email.subject.clone(),
        body: email.body.clone(),
        at_millis: now_millis(),
        read: true,
        outbound: true,
    };
    if let Err(err) = inbox.append(company, &record).await {
        tracing::warn!(
            company = %company,
            error = %err,
            "workflow delivery: failed to record the outbound email"
        );
    }
}

/// Whether the company's inbox already holds **inbound** mail from `to` — an
/// established thread.
///
/// Fails closed (`false`) on a missing sending address or an inbox read error,
/// which routes the caller to the cold-recipient skip.
///
/// Delegates the lookup to [`InboxStore::has_inbound_from`] rather than scanning
/// a page of [`messages`](InboxStore::messages): this is a security gate, and a
/// gate built on a capped oldest-first page silently stops finding real
/// correspondents once a company's inbox outgrows the cap (PR #226 review).
async fn recipient_is_established(
    inbox: &dyn InboxStore,
    company: &CompanyId,
    company_address: &str,
    to: &str,
) -> bool {
    if company_address.trim().is_empty() {
        return false;
    }
    let key = local_part(company_address);
    inbox
        .has_inbound_from(company, &key, to)
        .await
        .unwrap_or(false) // fail closed → the cold-recipient skip
}

/// Posts a report to the wired channel adapter with id `channel_id`.
///
/// `Err((reason, detail))` carries both halves the caller needs: `detail` is the
/// operator-readable text — an unwired id names what *is* wired, so the fix is
/// obvious from the run result alone — and `reason` is the classification that
/// may be logged. They are returned together because only this function knows
/// which of the two failure shapes happened; recovering it later from the string
/// is exactly the pattern-match-on-prose coupling issue #248 exists to avoid.
async fn post_to_channel(
    delivery: &WorkflowDeliveryDeps,
    record: &CompanyRecord,
    channel_id: &str,
    subject: &str,
    text: &str,
) -> Result<(), (DeliveryReason, String)> {
    // Since issue #1757 `operator` is a first-class, durable delivery channel
    // (see `deliverable_channel_ids`), so a `channel` destination may name it
    // like any desk — the post lands, journal-backed, in the standing Operator
    // channel. No target is refused here by name any more; an id nobody wired is
    // still caught below with the same sentence the console's picker pre-flight
    // shows (issue #981).
    send_to_channel_adapter(delivery, record, channel_id, subject, text, false).await
}

/// Posts an `owner` fallback report to the durable operator channel (issue
/// #1757).
///
/// The landing spot for an `owner` report the company cannot email — no mailbox,
/// or no admin with an address. A thin, named wrapper over
/// [`send_to_channel_adapter`] so the owner arm reads for what it is: the runtime
/// builder wires a
/// [`DurableOperatorChannel`](crate::runtime::channel::DurableOperatorChannel)
/// into the delivery adapter set under the `operator` id, so this finds a real,
/// journal-backed write path; a build that wired none degrades to a plain
/// "not wired" error and the caller reports the fallback as failed rather than
/// silently discarding it.
async fn post_to_operator(
    delivery: &WorkflowDeliveryDeps,
    record: &CompanyRecord,
    subject: &str,
    text: &str,
) -> Result<(), (DeliveryReason, String)> {
    // `admin_only: true` — this is precisely the report an unavailable mailbox
    // would otherwise have sent only to active administrators
    // (`owner_recipients` filters to `UserRole::Admin` + `UserStatus::Active`).
    // The channel fallback must not widen that audience just because mail
    // failed (issue #1781 review, Codex P1); see `OWNER_FALLBACK_REPORT_AUTHOR`.
    send_to_channel_adapter(
        delivery,
        record,
        crate::runtime::channel::OPERATOR_CHANNEL,
        subject,
        text,
        true,
    )
    .await
}

/// The header prefixed to every report that lands in the operator channel
/// (issue #1757).
const OPERATOR_REPORT_HEADER: &str = "Workflow report";

/// Formats a report for the operator channel with its source header, so the
/// aggregated "what happened" feed reads as scannable reports (each named by its
/// workflow and node) rather than a firehose of chat text (issue #1757).
fn operator_report(subject: &str, text: &str) -> String {
    format!("{OPERATOR_REPORT_HEADER} — {subject}\n\n{text}")
}

/// Finds the wired adapter with id `channel_id` and sends `subject`/`text` to
/// it. The shared core of [`post_to_channel`] and [`post_to_operator`].
///
/// A report bound for the operator channel gets a source header
/// ([`operator_report`]) so the aggregating surface stays scannable; every other
/// channel gets the plain `subject`/`text` a desk or provider expects.
///
/// `admin_only` marks an owner-fallback report so the read path can restrict it
/// to administrators (issue #1781 review, Codex P1) — see
/// [`OWNER_FALLBACK_REPORT_AUTHOR`](crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR).
/// Only `post_to_operator` ever sets it; `post_to_channel`'s explicit `channel`
/// destination always passes `false`, unchanged.
///
/// `record` resolves the **journal** address for the operator channel via
/// [`CompanyRecord::operator_feed_channel`] — ordinarily the same as
/// `channel_id`, except for a company whose roster grandfathers a teammate at
/// the literal `operator` id, where it diverts to a disjoint id so a report can
/// never land on that teammate's own DM (issue #1781 review, CodeRabbit Major +
/// Codex P2). `channel_id` itself still drives the **adapter lookup** below —
/// `DurableOperatorChannel::channel_id()` is always the literal `operator`, in
/// every company, so the lookup is unaffected by the divergence.
async fn send_to_channel_adapter(
    delivery: &WorkflowDeliveryDeps,
    record: &CompanyRecord,
    channel_id: &str,
    subject: &str,
    text: &str,
    admin_only: bool,
) -> Result<(), (DeliveryReason, String)> {
    let Some(adapter) = delivery
        .channels
        .iter()
        .find(|c| c.channel_id() == channel_id)
    else {
        // Name what IS deliverable, in the same sentence the console's
        // client-side pre-flight shows (issue #981), so the host and the picker
        // never disagree about which channels are real.
        let wired: Vec<&str> = delivery
            .channels
            .iter()
            .map(|c| c.channel_id())
            .collect::<Vec<_>>();
        return Err((
            DeliveryReason::ChannelNotWired,
            crate::runtime::channel::undeliverable_channel_message(channel_id, &wired),
        ));
    };
    let is_operator = channel_id == crate::runtime::channel::OPERATOR_CHANNEL;
    let body = if is_operator {
        operator_report(subject, text)
    } else {
        format!("{subject}\n\n{text}")
    };
    let journal_channel = if is_operator {
        let feed = record.operator_feed_channel();
        // Issue #1781 review (CodeRabbit P2 follow-up, then a fresh P2 on the
        // follow-up itself): `operator_feed_channel` diverts to
        // `OPERATOR_CHANNEL_COLLISION_FALLBACK` without re-checking that
        // address is itself free — a second grandfathered desk name can still
        // shadow it (see `operator_feed_channel_fallback_shadowed`'s own doc
        // for why no third address closes this). There is nowhere safe left to
        // journal this report: sending it to `feed` anyway would silently mix
        // it into that shadowing desk's own transcript while still reporting
        // `Sent`. Refuse instead of guessing — the operator can rename the
        // colliding desk and re-run, which a `Sent`-but-misrouted report would
        // never have surfaced a reason to do.
        if record.operator_feed_channel_fallback_shadowed() {
            tracing::error!(
                company = %record.id,
                fallback = feed,
                "operator feed collision-fallback channel is itself shadowed by a \
                 grandfathered desk name; refusing this workflow report instead of \
                 misrouting it into that desk's own transcript"
            );
            return Err((
                DeliveryReason::ChannelCollisionShadowed,
                format!(
                    "the operator feed's collision-fallback address (`{feed}`) is itself \
                     claimed by another desk's name, so this report has no safe address to \
                     journal to — rename the colliding desk to clear this"
                ),
            ));
        }
        feed
    } else {
        channel_id
    };
    adapter
        .send(OutboundMessage {
            message_id: None,
            task_id: None,
            channel: journal_channel.to_string(),
            agent: admin_only.then(|| crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR.to_string()),
            text: body,
            steps: Vec::new(),
            reply_to: None,
            mentions: Vec::new(),
        })
        .await
        // `err` is the adapter's own words. Same rule as mail: it rides
        // `detail`, never the classification (issue #248).
        .map_err(|err| {
            (
                DeliveryReason::ChannelRefused,
                format!("the channel refused the message: {err}"),
            )
        })
}

/// Whether the run's output carries an entry for `node_id` — i.e. the engine
/// actually reached that node.
fn node_was_reached(output: &Value, node_id: &str) -> bool {
    !output
        .get("nodes")
        .and_then(|nodes| nodes.get(node_id))
        .unwrap_or(&Value::Null)
        .is_null()
}

/// The report body for one output node: every item's text, in order.
///
/// The engine emits items as `{"json": {...}}`, and the `text` an agent node
/// produced sometimes sits one level deeper (`json.json.text`) — the same
/// double-wrapping the console's run-result parser handles, so the outer value
/// wins here too. An item carrying no readable text falls back to its compact
/// JSON, so a data-shaped report is delivered rather than dropped.
fn report_text(output: &Value, node_id: &str) -> String {
    let items = output
        .get("nodes")
        .and_then(|nodes| nodes.get(node_id))
        .and_then(|node| node.get("items"))
        .and_then(Value::as_array);
    let Some(items) = items else {
        return "(this workflow step produced no output)".to_string();
    };

    let mut parts: Vec<String> = Vec::new();
    for item in items {
        let json = item.get("json").unwrap_or(item);
        if let Some(text) = read_nested_str(json, "text") {
            parts.push(text.to_string());
        } else {
            parts.push(json.to_string());
        }
    }
    if parts.is_empty() {
        return "(this workflow step produced no output)".to_string();
    }
    truncate_chars(&parts.join("\n\n"), MAX_REPORT_CHARS)
}

/// Reads a string field from an item's `json`, preferring the outermost value
/// and falling back to the nested `json.json.<key>` the engine sometimes emits.
fn read_nested_str<'a>(json: &'a Value, key: &str) -> Option<&'a str> {
    let non_empty = |v: &'a Value| v.as_str().filter(|s| !s.trim().is_empty());
    if let Some(outer) = json.get(key).and_then(non_empty) {
        return Some(outer);
    }
    json.get("json")
        .and_then(|inner| inner.get(key))
        .and_then(non_empty)
}

/// Truncates `text` to at most `max` characters, appending a visible marker when
/// it actually cut something.
///
/// Character-indexed on purpose: slicing a `String` by byte offset panics when
/// the offset lands mid-codepoint, and a report can carry any UTF-8 the run
/// produced.
fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((byte_index, _)) => format!("{}{TRUNCATION_MARKER}", &text[..byte_index]),
    }
}

/// The subject line one report carries: the company, the workflow, and which
/// step reported.
fn subject_for(record: &CompanyRecord, workflow: &WorkflowFile, node_name: &str) -> String {
    format!(
        "[{}] {} — {}",
        record.manifest.company.name, workflow.name, node_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;

    use crate::company::parse_workflow;
    use crate::error::OpenCompanyError;
    use crate::policy::ManifestApprovalGate;
    use crate::ports::UserRecord;
    use crate::ports::types::CompanyId;
    use crate::ports::types::SecretValue;
    use crate::runtime::channel::{
        DeskChannel, DurableOperatorChannel, OPERATOR_CHANNEL, OperatorChannel,
    };
    use crate::server::ops::mailer::{MailSender, RecordingMailSender};
    use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};
    use crate::store::{FsInboxStore, FsOps};

    /// The company's own sending address in every test below.
    const COMPANY_ADDRESS: &str = "acme@opencompany.test";

    /// A graph whose single `output` node carries `destination`, wired
    /// `trigger → done`. `target` is omitted when `None`.
    fn graph(kind: &str, target: Option<&str>) -> WorkflowFile {
        let target_line = target
            .map(|t| format!("target = \"{t}\"\n"))
            .unwrap_or_default();
        let src = format!(
            r#"
id = "report_flow"
name = "Report flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Owner summary"
[node.destination]
kind = "{kind}"
{target_line}
[[edge]]
from = "start"
to = "done"
"#
        );
        parse_workflow(&src).expect("test graph is valid")
    }

    /// The same graph with **no** `[node.destination]` stanza at all — the
    /// pre-#170 shape every seeded company template still ships (issue #925).
    fn graph_without_destination() -> WorkflowFile {
        let src = r#"
id = "report_flow"
name = "Report flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Owner summary"
[[edge]]
from = "start"
to = "done"
"#;
        parse_workflow(src).expect("a graph whose output node names no destination is still valid")
    }

    /// A run output in which `done` produced one text item — the reached case.
    fn reached_output() -> Value {
        serde_json::json!({
            "nodes": {
                "start": { "items": [{ "json": { "seed": 1 } }] },
                "done": { "items": [{ "json": { "text": "Q3 is up 12%." } }] },
            }
        })
    }

    /// A company record whose `[tools].allow` is exactly `grants`.
    fn record(grants: &[&str]) -> CompanyRecord {
        let allow = grants
            .iter()
            .map(|g| format!("\"{g}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = toml::from_str(&format!(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[tools]
allow = [{allow}]
"#
        ))
        .expect("valid manifest");
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }
    }

    fn smtp_creds() -> SmtpCredentials {
        SmtpCredentials {
            host: "smtp.example.test".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "acme".into(),
            password: SecretValue("hunter2".into()),
            from_name: "Acme".into(),
            from_email: COMPANY_ADDRESS.into(),
        }
    }

    /// A [`MailSender`] that always refuses, for the "a send failure does not
    /// fail the run" case.
    struct RefusingMailSender;

    #[async_trait]
    impl MailSender for RefusingMailSender {
        async fn send(
            &self,
            _creds: &MailCredentials,
            _email: &OutboundEmail,
        ) -> Result<(), OpenCompanyError> {
            Err(OpenCompanyError::Config("smtp said no".into()))
        }
    }

    /// The offline delivery bundle: a recording mail sender (or none), tempdir
    /// inbox + user stores, and the built-in operator channel.
    struct Harness {
        deps: WorkflowDeliveryDeps,
        mail: RecordingMailSender,
        channel: OperatorChannel,
        /// A durable-looking channel, present only when
        /// [`with_recording_channel`](Harness::with_recording_channel) wired
        /// one. Needed by any case whose subject is what happens AFTER a send
        /// succeeds: `operator` is refused before the send, so it can no longer
        /// stand in for a channel that works.
        recording: Option<crate::runtime::channel::RecordingChannel>,
        inbox: Arc<FsInboxStore>,
        users: Arc<FsOps>,
        company: CompanyId,
        /// The approvals queue, when [`with_parking`](Harness::with_parking)
        /// wired one. The real gate and a real on-disk journal, not fakes: the
        /// point of these tests is that a workflow's park lands in the same
        /// queue an agent's does.
        gate: Option<Arc<ManifestApprovalGate>>,
        journal: Option<Arc<RuntimeJournal>>,
        /// The real on-disk event journal the write-behind delivery record
        /// (issue #529) lands in — held so a test can read back the
        /// [`CompanyEvent::WorkflowReportDelivered`] lines a dispatch appended.
        events: Arc<dyn EventLog>,
    }

    impl Harness {
        fn new(dir: &std::path::Path, with_mail: bool, with_channel: bool) -> Self {
            let mail = RecordingMailSender::new();
            let inbox = Arc::new(FsInboxStore::new(dir));
            let users = Arc::new(FsOps::new(dir));
            // The interactive in-memory operator buffer, kept so a test can
            // assert it stays untouched — workflow delivery must never route to
            // it. It is deliberately NOT wired into `deps.channels`.
            let channel = OperatorChannel::new();
            // A real filesystem journal, like the outcome tests use: the write
            // side must actually land on disk and read back, so the delivered
            // ledger is exercised end to end rather than against a double.
            let events: Arc<dyn EventLog> = Arc::new(crate::store::FsEventLog::new(dir));
            // The delivery adapter set the production builder wires (issue #1757):
            // the DURABLE operator channel, journaling into the event log, is the
            // owner/no-mailbox fallback's landing spot. `with_channel = false`
            // wires nothing, so the fallback has no operator adapter and reports a
            // failure row — the misconfigured-build case.
            let channels: Vec<Arc<dyn ChannelAdapter>> = if with_channel {
                vec![Arc::new(DurableOperatorChannel::new(
                    CompanyId::new("acme"),
                    events.clone(),
                ))]
            } else {
                Vec::new()
            };
            Self {
                deps: WorkflowDeliveryDeps {
                    events: events.clone(),
                    mail: with_mail.then(|| CompanyMail {
                        sender: Arc::new(mail.clone()),
                        smtp: smtp_creds(),
                    }),
                    inbox: inbox.clone(),
                    users: users.clone(),
                    bootstrap_admin: None,
                    channels,
                    parking: None,
                },
                mail,
                channel,
                recording: None,
                inbox,
                users,
                company: CompanyId::new("acme"),
                gate: None,
                journal: None,
                events,
            }
        }

        /// Sets the deployment's standing bootstrap-admin address (M8), the same
        /// value the production builder threads from `AppConfig::bootstrap_admin`.
        fn with_bootstrap_admin(mut self, email: &str) -> Self {
            self.deps.bootstrap_admin = Some(email.to_string());
            self
        }

        /// Sets the company record's manifest so a test can name `[users] admins`
        /// standing invites. Rebuilt from TOML rather than mutated field-by-field
        /// so the parse mirrors a real manifest load.
        fn manifest_with_admins(admins: &[&str]) -> crate::company::CompanyManifest {
            let list = admins
                .iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ");
            toml::from_str(&format!(
                r#"
[company]
name = "Acme"

[policy]
mode = "full"

[users]
admins = [{list}]
"#
            ))
            .expect("valid manifest with [users] admins")
        }

        /// Wires the approvals queue the production builder wires: a real
        /// [`ManifestApprovalGate`] over `policy_mode` and a real
        /// [`RuntimeJournal`] on disk under `dir`.
        ///
        /// `policy_mode` is a parameter because `full` is the interesting one:
        /// it is the mode under which `evaluate` would return `Allow` for a
        /// `Send` effect, so a test that parks under `full` is the one that
        /// proves delivery does not route through `evaluate`.
        fn with_parking(mut self, dir: &std::path::Path, policy_mode: &str) -> Self {
            let policy = toml::from_str(&format!("mode = \"{policy_mode}\"\n"))
                .expect("valid [policy] block");
            let gate = Arc::new(ManifestApprovalGate::new(policy));
            let journal = Arc::new(RuntimeJournal::new(dir.join("journal.jsonl")));
            self.deps.parking = Some(DeliveryParking {
                approvals: gate.clone(),
                journal: journal.clone(),
                // Issue #978: a test fixture parks into its own queues. The
                // production wiring is `RuntimeBuilder`, which hands the
                // runtime's own handles in so a park arms what the resolve
                // path releases.
                continuations: Default::default(),
                gates: Default::default(),
                blocked_nodes: Default::default(),
            });
            self.gate = Some(gate);
            self.journal = Some(journal);
            self
        }

        /// Wires a gate plus a journal whose every write **fails**, for the
        /// partial-failure case.
        ///
        /// The failure is induced by pointing the journal at a path that is
        /// already a *directory*: `append` creates the parent fine and then
        /// `OpenOptions::open` returns `EISDIR`. Deterministic, cross-platform,
        /// and it fails at the real I/O boundary rather than at a mock, so the
        /// test exercises the same error path a full disk would.
        fn with_failing_journal(mut self, dir: &std::path::Path, policy_mode: &str) -> Self {
            let policy = toml::from_str(&format!("mode = \"{policy_mode}\"\n"))
                .expect("valid [policy] block");
            let gate = Arc::new(ManifestApprovalGate::new(policy));
            let blocked = dir.join("unwritable-journal.jsonl");
            std::fs::create_dir_all(&blocked).expect("journal path occupied by a directory");
            let journal = Arc::new(RuntimeJournal::new(blocked));
            self.deps.parking = Some(DeliveryParking {
                approvals: gate.clone(),
                journal: journal.clone(),
                // Issue #978: a test fixture parks into its own queues. The
                // production wiring is `RuntimeBuilder`, which hands the
                // runtime's own handles in so a park arms what the resolve
                // path releases.
                continuations: Default::default(),
                gates: Default::default(),
                blocked_nodes: Default::default(),
            });
            self.gate = Some(gate);
            self.journal = Some(journal);
            self
        }

        /// Adds an active admin with `email` to the company directory.
        async fn add_admin(&self, id: &str, email: &str) {
            self.users
                .upsert_user(
                    &self.company,
                    &UserRecord {
                        id: id.to_string(),
                        email: email.to_string(),
                        display_name: None,
                        avatar: None,
                        role: UserRole::Admin,
                        status: UserStatus::Active,
                        password_hash: None,
                        must_change_password: false,
                        created_at_millis: 1,
                        last_seen_at_millis: None,
                        updated_at_millis: 1,
                    },
                )
                .await
                .expect("user upserted");
        }

        /// Files an INBOUND email from `from`, which is what makes that address
        /// an established thread.
        async fn receive_from(&self, from: &str) {
            self.inbox
                .append(
                    &self.company,
                    &EmailRecord {
                        id: generate_id(),
                        inbox: local_part(COMPANY_ADDRESS),
                        from_name: String::new(),
                        from_email: from.to_string(),
                        subject: "hello".to_string(),
                        body: "hi".to_string(),
                        at_millis: 1,
                        read: false,
                        outbound: false,
                    },
                )
                .await
                .expect("inbound filed");
        }

        /// Every message in the company's own inbox.
        async fn inbox_messages(&self) -> Vec<EmailRecord> {
            self.inbox
                .messages(&self.company, &local_part(COMPANY_ADDRESS), 100, 0)
                .await
                .expect("inbox readable")
        }

        /// Every `WorkflowReportDelivered` the write-behind path journaled
        /// (issue #529) — what a re-run's fold would later read back.
        async fn journaled_deliveries(&self) -> Vec<CompanyEvent> {
            self.events
                .read_from(
                    &self.company,
                    crate::ports::types::EventSeq::new(0),
                    usize::MAX,
                )
                .await
                .expect("journal readable")
                .into_iter()
                .map(|s| s.event)
                .filter(|e| matches!(e, CompanyEvent::WorkflowReportDelivered { .. }))
                .collect()
        }

        /// The text of every workflow report the durable operator channel
        /// journaled (issue #1757): `AgentReply`s authored by `workflow` on the
        /// dedicated `operator` line the standing Operator channel renders. This
        /// is what proves the owner/no-mailbox fallback is a real, readable
        /// delivery rather than a discard on the in-memory buffer.
        async fn operator_reports(&self) -> Vec<String> {
            self.events
                .read_from(
                    &self.company,
                    crate::ports::types::EventSeq::new(0),
                    usize::MAX,
                )
                .await
                .expect("journal readable")
                .into_iter()
                .filter_map(|s| match s.event {
                    CompanyEvent::AgentReply {
                        chat_id,
                        agent_id,
                        text,
                        ..
                    } if chat_id == crate::runtime::channel::OPERATOR_CHANNEL
                        // `WORKFLOW_REPLY_AUTHOR` covers an explicit `channel`
                        // destination's report; `OWNER_FALLBACK_REPORT_AUTHOR`
                        // covers the `owner`-with-no-mailbox fallback (issue
                        // #1781 review, Codex P1) — both are still genuine,
                        // durable operator-channel reports, just gated
                        // differently on read. A test that cares which one
                        // landed reads `agent_id` itself via
                        // `operator_report_authors`.
                        && (agent_id == crate::runtime::channel::WORKFLOW_REPLY_AUTHOR
                            || agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR) =>
                    {
                        Some(text)
                    }
                    _ => None,
                })
                .collect()
        }

        /// Every operator-channel `AgentReply`'s `(agent_id, text)` pair, for a
        /// test that needs to tell an owner-fallback report apart from an
        /// ordinary one rather than just counting them (issue #1781 review,
        /// Codex P1).
        async fn operator_report_authors(&self) -> Vec<(String, String)> {
            self.events
                .read_from(
                    &self.company,
                    crate::ports::types::EventSeq::new(0),
                    usize::MAX,
                )
                .await
                .expect("journal readable")
                .into_iter()
                .filter_map(|s| match s.event {
                    CompanyEvent::AgentReply {
                        chat_id,
                        agent_id,
                        text,
                        ..
                    } if chat_id == crate::runtime::channel::OPERATOR_CHANNEL => {
                        Some((agent_id, text))
                    }
                    _ => None,
                })
                .collect()
        }

        /// Swaps the delivery bundle's event journal for one whose every append
        /// **fails**, for the "a journal failure does not fail delivery" case.
        fn with_failing_events(mut self) -> Self {
            self.deps.events = Arc::new(FailingEventLog);
            self
        }

        /// Wires a channel that accepts a send, under an ordinary channel id.
        ///
        /// The operator channel used to serve this purpose, but delivery now
        /// refuses it outright, which lands the caller in the refusal branch
        /// before the behaviour under test is reached. Anything that asserts
        /// what follows a successful send needs this instead.
        fn with_recording_channel(mut self, id: &str) -> Self {
            let channel = crate::runtime::channel::RecordingChannel::new(id);
            self.deps.channels.push(Arc::new(channel.clone()));
            self.recording = Some(channel);
            self
        }

        /// The channel [`with_recording_channel`](Harness::with_recording_channel) wired.
        fn recording(&self) -> &crate::runtime::channel::RecordingChannel {
            self.recording
                .as_ref()
                .expect("with_recording_channel was not called")
        }
    }

    /// An [`EventLog`] whose `append` always errors — the write-behind delivery
    /// record's failure path (issue #529). Reads yield nothing; the point is the
    /// append, and that a delivery survives it.
    struct FailingEventLog;

    #[async_trait]
    impl EventLog for FailingEventLog {
        async fn append(
            &self,
            _company: &CompanyId,
            _event: CompanyEvent,
        ) -> crate::Result<crate::ports::types::EventSeq> {
            Err(OpenCompanyError::Config(
                "event journal is unwritable".into(),
            ))
        }

        async fn read_from(
            &self,
            _company: &CompanyId,
            _seq: crate::ports::types::EventSeq,
            _limit: usize,
        ) -> crate::Result<Vec<crate::ports::types::StoredEvent>> {
            Ok(Vec::new())
        }

        fn subscribe(
            &self,
            _company: &CompanyId,
        ) -> futures::stream::BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(futures::stream::empty())
        }
    }

    /// A [`UserStore`] whose `list_users` always errors — the M8 store-error
    /// path. Every other method is unreachable for these tests (the `owner`
    /// resolver reads only `list_users`) and panics if a future caller leans on
    /// it, rather than quietly returning an empty result that would hide a bug.
    struct FailingUserStore;

    #[async_trait]
    impl UserStore for FailingUserStore {
        async fn list_users(&self, _company: &CompanyId) -> crate::Result<Vec<UserRecord>> {
            Err(OpenCompanyError::Config(
                "user directory is unreadable".into(),
            ))
        }
        async fn get_user(
            &self,
            _company: &CompanyId,
            _id: &str,
        ) -> crate::Result<Option<UserRecord>> {
            unreachable!("owner delivery reads only list_users")
        }
        async fn find_user_by_email(
            &self,
            _company: &CompanyId,
            _email: &str,
        ) -> crate::Result<Option<UserRecord>> {
            unreachable!("owner delivery reads only list_users")
        }
        async fn upsert_user(&self, _company: &CompanyId, _user: &UserRecord) -> crate::Result<()> {
            unreachable!("owner delivery reads only list_users")
        }
        async fn delete_user(&self, _company: &CompanyId, _id: &str) -> crate::Result<bool> {
            unreachable!("owner delivery reads only list_users")
        }
        async fn list_invites(
            &self,
            _company: &CompanyId,
        ) -> crate::Result<Vec<crate::ports::InviteRecord>> {
            unreachable!("owner delivery reads only list_users")
        }
        async fn find_invite_by_email(
            &self,
            _company: &CompanyId,
            _email: &str,
        ) -> crate::Result<Option<crate::ports::InviteRecord>> {
            unreachable!("owner delivery reads only list_users")
        }
        async fn upsert_invite(
            &self,
            _company: &CompanyId,
            _invite: &crate::ports::InviteRecord,
        ) -> crate::Result<()> {
            unreachable!("owner delivery reads only list_users")
        }
        async fn mark_invite_notified(
            &self,
            _company: &CompanyId,
            _id: &str,
            _at_millis: u64,
        ) -> crate::Result<bool> {
            unreachable!("owner delivery reads only list_users")
        }
        async fn delete_invite(&self, _company: &CompanyId, _id: &str) -> crate::Result<bool> {
            unreachable!("owner delivery reads only list_users")
        }
    }

    // --- owner ---------------------------------------------------------------

    /// `owner` resolves to the company's active admins server-side and emails
    /// each of them. The graph named nobody — that is the whole point.
    #[tokio::test]
    async fn owner_emails_every_active_admin() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        h.add_admin("u1", "ada@acme.test").await;
        h.add_admin("u2", "grace@acme.test").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 2, "{reports:?}");
        assert!(reports.iter().all(|r| r.status == DeliveryStatus::Sent));
        let mut addressed: Vec<String> = h.mail.sent().into_iter().map(|(_, e)| e.to).collect();
        addressed.sort();
        assert_eq!(addressed, vec!["ada@acme.test", "grace@acme.test"]);
        // The report body is the output node's text, and the subject names the
        // company, the workflow, and the step.
        let (_, email) = &h.mail.sent()[0];
        assert!(email.body.contains("Q3 is up 12%."), "{}", email.body);
        assert!(email.subject.contains("Acme"), "{}", email.subject);
        assert!(email.subject.contains("Report flow"), "{}", email.subject);
        // `owner` needs no grant: this record grants nothing at all.
    }

    /// A suspended admin and a plain member are not the owner. Only active
    /// admins are.
    #[tokio::test]
    async fn owner_ignores_suspended_admins_and_members() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        h.add_admin("u1", "ada@acme.test").await;
        for (id, email, role, status) in [
            (
                "u2",
                "sus@acme.test",
                UserRole::Admin,
                UserStatus::Suspended,
            ),
            ("u3", "mem@acme.test", UserRole::Member, UserStatus::Active),
        ] {
            h.users
                .upsert_user(
                    &h.company,
                    &UserRecord {
                        id: id.to_string(),
                        email: email.to_string(),
                        display_name: None,
                        avatar: None,
                        role,
                        status,
                        password_hash: None,
                        must_change_password: false,
                        created_at_millis: 1,
                        last_seen_at_millis: None,
                        updated_at_millis: 1,
                    },
                )
                .await
                .unwrap();
        }

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(h.mail.sent().len(), 1);
        assert_eq!(h.mail.sent()[0].1.to, "ada@acme.test");
    }

    /// With no mailbox wired, `owner` falls back to the DURABLE operator channel
    /// (issue #1757): a genuine, journal-backed delivery — not the discard-on-an-
    /// in-memory-buffer failure it used to report. The report lands in the event
    /// log on the dedicated Operator line, and the interactive buffer is
    /// untouched.
    #[tokio::test]
    async fn owner_falls_back_to_the_durable_operator_channel_without_mail() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, true);
        h.add_admin("u1", "ada@acme.test").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
        assert_eq!(reports[0].target.as_deref(), Some(OPERATOR_CHANNEL));
        assert!(reports[0].detail.contains("no mailbox"), "{reports:?}");
        assert_eq!(reports[0].reason, DeliveryReason::OwnerFellBackNoMailbox);
        // The interactive in-memory buffer is never a delivery surface.
        assert!(h.channel.sent().is_empty());
        // The report is durable: an `AgentReply` landed on the dedicated
        // Operator line — never the General desk — carrying the workflow's
        // subject header so it reads as a workflow report, not an agent's own
        // reply.
        let landed = h.operator_reports().await;
        assert_eq!(landed.len(), 1, "the report must be journaled: {landed:?}");
        assert!(landed[0].contains("Q3 is up 12%."), "{landed:?}");
        assert!(landed[0].contains("Report flow"), "{landed:?}");
    }

    /// Issue #1781 review (Codex P1): the `owner`-with-no-mailbox fallback must
    /// journal under a distinct author from an ordinary operator-channel report,
    /// so the read path (`server::chat_history::history_for_desk`) can restrict
    /// exactly this row to administrators — the same audience the sibling email
    /// branch already enforces (`owner_recipients` filters to active admins).
    ///
    /// Proven against a **contrasting pair** in the same test rather than just
    /// asserting the fallback's author: an explicit `channel` destination
    /// naming `operator` is a workflow author's deliberate choice, general
    /// audience, and must keep the ordinary `WORKFLOW_REPLY_AUTHOR` — this is
    /// what shows the fallback's marker is additive, not a wholesale change to
    /// every operator-channel report.
    #[tokio::test]
    async fn owner_fallback_report_is_authored_distinctly_from_an_ordinary_one() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, true);
        h.add_admin("u1", "ada@acme.test").await;

        deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;
        deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("channel", Some(OPERATOR_CHANNEL)),
            "run-2",
            &reached_output(),
            &[],
        )
        .await;

        let authors = h.operator_report_authors().await;
        assert_eq!(authors.len(), 2, "{authors:?}");
        assert!(
            authors
                .iter()
                .any(|(agent_id, _)| agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR),
            "the owner fallback must be marked distinctly: {authors:?}"
        );
        assert!(
            authors
                .iter()
                .any(|(agent_id, _)| agent_id == crate::runtime::channel::WORKFLOW_REPLY_AUTHOR),
            "an explicit `channel: operator` destination must keep the ordinary \
             author — the marker is additive, not a wholesale change: {authors:?}"
        );
    }

    /// Issue #1781 review (CodeRabbit Major + Codex P2): a company whose roster
    /// already grandfathers a **teammate** at the literal id `operator` (no desk
    /// of the same id — see `CompanyRecord::operator_feed_channel`) must not
    /// have the durable Operator system feed land on that same address. Proven
    /// for **both** report shapes that can reach the operator channel — the
    /// `owner` fallback and an explicit `channel: operator` destination — since
    /// review found the collision on the desk-list/read side, not the write
    /// guard, and either shape re-opens it if only one were fixed.
    ///
    /// Pre-fix, both reports journaled at `chat_id == OPERATOR_CHANNEL`
    /// (`"operator"`) — exactly the address `ChatView` addresses that teammate's
    /// own DM by (issue #364). This test's whole point is that the two lines
    /// now diverge.
    #[tokio::test]
    async fn a_report_diverts_off_a_grandfathered_teammates_own_operator_line() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, true);
        h.add_admin("u1", "ada@acme.test").await;

        let mut collided = record(&[]);
        collided.manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "operator"
role = "Chief of Staff"
"#,
        )
        .expect("valid manifest with a grandfathered `operator` teammate");
        assert!(
            collided.is_roster_agent(OPERATOR_CHANNEL) && !collided.desk_exists(OPERATOR_CHANNEL),
            "fixture must actually be in the collision state this test exercises"
        );

        deliver_outputs(
            Some(&h.deps),
            &collided,
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;
        deliver_outputs(
            Some(&h.deps),
            &collided,
            &graph("channel", Some(OPERATOR_CHANNEL)),
            "run-2",
            &reached_output(),
            &[],
        )
        .await;

        let landed = h
            .events
            .read_from(
                &h.company,
                crate::ports::types::EventSeq::new(0),
                usize::MAX,
            )
            .await
            .expect("journal readable")
            .into_iter()
            .filter_map(|s| match s.event {
                CompanyEvent::AgentReply { chat_id, .. } => Some(chat_id),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(landed.len(), 2, "{landed:?}");
        assert!(
            landed
                .iter()
                .all(|chat_id| chat_id == crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK),
            "every report bound for the system feed must land off the \
             grandfathered teammate's own `operator` line, not on it: {landed:?}"
        );
        assert!(
            landed.iter().all(|chat_id| chat_id != OPERATOR_CHANNEL),
            "the literal `operator` line must stay untouched by the durable \
             feed — that is the teammate's own DM address: {landed:?}"
        );
    }

    /// Issue #1781 review (fresh P2 on `operator_feed_channel_fallback_shadowed`
    /// itself): the residual double collision that predicate detects must not
    /// merely be logged while the report ships anyway. Reuses this fixture's
    /// manifest shape from `CompanyRecord`'s own
    /// `operator_feed_channel_fallback_shadowed_detects_a_double_collision` test
    /// (`ports::types`) — one grandfathered desk named "Operator" (shadowing the
    /// primary address) and a second, different desk named "operator-feed"
    /// (shadowing the collision fallback) — and proves the delivery layer
    /// refuses the send rather than journaling into that second desk's own
    /// transcript while still reporting `Sent`.
    #[tokio::test]
    async fn a_double_collision_refuses_delivery_instead_of_misrouting() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, true);

        let mut collided = record(&[]);
        collided.manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[group_chat]]
id = "legacy_ops"
name = "Operator"
members = []

[[group_chat]]
id = "ops2"
name = "operator-feed"
members = []
"#,
        )
        .expect("valid manifest with a double grandfathered collision");
        assert!(
            collided.operator_feed_channel_fallback_shadowed(),
            "fixture must actually be in the double-collision state this test \
             exercises, or it proves nothing"
        );

        let reports = deliver_outputs(
            Some(&h.deps),
            &collided,
            &graph("channel", Some(OPERATOR_CHANNEL)),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(
            reports[0].status,
            DeliveryStatus::Failed,
            "a shadowed fallback must be reported as a failed delivery, never \
             `Sent` — a `Sent` row here is exactly the silent misroute this test \
             guards against: {reports:?}"
        );
        assert_eq!(
            reports[0].reason,
            DeliveryReason::ChannelCollisionShadowed,
            "{reports:?}"
        );

        let landed = h
            .events
            .read_from(
                &h.company,
                crate::ports::types::EventSeq::new(0),
                usize::MAX,
            )
            .await
            .expect("journal readable")
            .into_iter()
            .filter_map(|s| match s.event {
                CompanyEvent::AgentReply { chat_id, .. } => Some(chat_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            landed.is_empty(),
            "a refused delivery must not append anything to the event log — in \
             particular nothing must land on \"operator-feed\", the second \
             desk's own transcript: {landed:?}"
        );
    }

    /// A company with a mailbox but no admin address also delivers durably to the
    /// operator channel rather than failing — the report still reaches the one
    /// human who could act on it.
    #[tokio::test]
    async fn owner_falls_back_to_the_durable_operator_channel_when_no_admin_has_an_address() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
        assert_eq!(
            reports[0].reason,
            DeliveryReason::OwnerFellBackNoAdminAddress
        );
        assert!(reports[0].detail.contains("no active admin"), "{reports:?}");
        assert!(h.mail.sent().is_empty(), "nothing should have been emailed");
        assert!(h.channel.sent().is_empty());
        let landed = h.operator_reports().await;
        assert_eq!(landed.len(), 1, "the report must be journaled: {landed:?}");
    }

    /// Both fallbacks unavailable: no mail, no operator channel wired at all
    /// (a misconfigured build). Still a row — `failed`, naming the gap — never
    /// silence.
    #[tokio::test]
    async fn owner_with_neither_mail_nor_a_channel_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, false);

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Failed);
        assert!(reports[0].detail.contains("operator"), "{reports:?}");
    }

    // --- owner: standing admin invites (issue #661 / M8) ---------------------

    /// **The M8 headline.** A fresh platform-provisioned tenant has nobody in
    /// its manifest and nobody in the user store yet, but the platform injected
    /// a bootstrap admin. An `owner` report must reach that address — not fall
    /// back to the operator channel, which is the one human who could act on it
    /// never hearing about it.
    #[tokio::test]
    async fn owner_emails_the_standing_bootstrap_admin_on_a_fresh_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true).with_bootstrap_admin("founder@acme.test");
        // No admins in the store, no `[users] admins` in the manifest.

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
        assert_eq!(reports[0].reason, DeliveryReason::OwnerEmailed);
        assert_eq!(reports[0].target.as_deref(), Some("founder@acme.test"));
        assert_eq!(h.mail.sent().len(), 1);
        assert_eq!(h.mail.sent()[0].1.to, "founder@acme.test");
        // The operator channel must be untouched — the whole bug is that the
        // report fell back to it.
        assert!(
            h.channel.sent().is_empty(),
            "the standing admin was mailed, so nothing goes to the operator channel"
        );
        // The send is mirrored into the inbox as outbound, and journaled.
        let outbound: Vec<_> = h
            .inbox_messages()
            .await
            .into_iter()
            .filter(|m| m.outbound)
            .collect();
        assert_eq!(outbound.len(), 1, "the send must leave an audit record");
        let journaled = h.journaled_deliveries().await;
        assert_eq!(journaled.len(), 1, "{journaled:?}");
    }

    /// A manifest `[users] admins` entry is a standing invite too, and is mailed
    /// the same way — even before that person has ever signed in.
    #[tokio::test]
    async fn owner_emails_a_manifest_admin_standing_invite() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        let mut rec = record(&[]);
        rec.manifest = Harness::manifest_with_admins(&["grace@acme.test"]);

        let reports = deliver_outputs(
            Some(&h.deps),
            &rec,
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
        assert_eq!(reports[0].reason, DeliveryReason::OwnerEmailed);
        assert_eq!(h.mail.sent().len(), 1);
        assert_eq!(h.mail.sent()[0].1.to, "grace@acme.test");
    }

    /// **User-record-wins.** A bootstrap admin who has since signed in and been
    /// *suspended* is not mailed through the leftover standing invite: their
    /// record wins, and a suspended admin is not an active one. `owner` then has
    /// no address to email and falls back to the durable operator channel with
    /// the M8 wording — a real delivery (issue #1757), not the failure it once
    /// reported.
    #[tokio::test]
    async fn owner_does_not_email_a_suspended_bootstrap_admin() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true).with_bootstrap_admin("founder@acme.test");
        // The bootstrap admin signed in, then was suspended: a record exists.
        h.users
            .upsert_user(
                &h.company,
                &UserRecord {
                    id: "founder".to_string(),
                    email: "founder@acme.test".to_string(),
                    display_name: None,
                    avatar: None,
                    role: UserRole::Admin,
                    status: UserStatus::Suspended,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: 1,
                    last_seen_at_millis: None,
                    updated_at_millis: 1,
                },
            )
            .await
            .unwrap();

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
        assert_eq!(
            reports[0].reason,
            DeliveryReason::OwnerFellBackNoAdminAddress
        );
        assert!(
            reports[0].detail.contains("standing admin invite"),
            "the fallback wording must name standing invites now: {reports:?}"
        );
        assert!(
            h.mail.sent().is_empty(),
            "a suspended admin must not be mailed, invite or not"
        );
        assert!(
            h.channel.sent().is_empty(),
            "the interactive operator buffer is not delivery"
        );
        // The report still lands, durably, on the operator channel.
        assert_eq!(h.operator_reports().await.len(), 1);
    }

    /// **Dedupe.** An address named both as an active admin and as the bootstrap
    /// admin is one person, and is mailed exactly once.
    #[tokio::test]
    async fn owner_dedupes_an_active_admin_that_is_also_the_bootstrap_admin() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true).with_bootstrap_admin("ada@acme.test");
        h.add_admin("u1", "ada@acme.test").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "one recipient, one row: {reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent);
        assert_eq!(h.mail.sent().len(), 1, "mailed once, not twice");
        assert_eq!(h.mail.sent()[0].1.to, "ada@acme.test");
    }

    /// A manifest admin address is normalized the same way the login path
    /// normalizes it, so `Grace@ACME.test` and `grace@acme.test` are one address
    /// — the send goes to the normalized form.
    #[tokio::test]
    async fn owner_normalizes_a_manifest_admin_address() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        let mut rec = record(&[]);
        rec.manifest = Harness::manifest_with_admins(&["Grace@ACME.test"]);

        let reports = deliver_outputs(
            Some(&h.deps),
            &rec,
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
        assert_eq!(h.mail.sent()[0].1.to, "grace@acme.test");
    }

    /// **Store-error stance (the M8 bug's worst case).** When the user store
    /// cannot be read, the standing invites are mailed anyway — dropping the only
    /// humans the company is known to have back to the operator channel is
    /// exactly the silent drop M8 fixes.
    #[tokio::test]
    async fn owner_still_emails_standing_invites_when_the_user_store_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = Harness::new(dir.path(), true, true).with_bootstrap_admin("founder@acme.test");
        h.deps.users = Arc::new(FailingUserStore);

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(
            reports[0].status,
            DeliveryStatus::Sent,
            "an unreadable store must still mail the standing invite: {reports:?}"
        );
        assert_eq!(reports[0].reason, DeliveryReason::OwnerEmailed);
        assert_eq!(h.mail.sent().len(), 1);
        assert_eq!(h.mail.sent()[0].1.to, "founder@acme.test");
    }

    // --- email ---------------------------------------------------------------

    /// The happy path: granted AND established. The mail goes out and is
    /// mirrored into the company inbox as outbound, for audit.
    #[tokio::test]
    async fn email_granted_and_established_sends_and_records_outbound() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        h.receive_from("ada@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["email.send"]),
            &graph("email", Some("ada@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent);
        assert_eq!(h.mail.sent().len(), 1);
        assert_eq!(h.mail.sent()[0].1.to, "ada@example.com");

        let messages = h.inbox_messages().await;
        let outbound: Vec<&EmailRecord> = messages.iter().filter(|m| m.outbound).collect();
        assert_eq!(outbound.len(), 1, "the send must leave an audit record");
        assert!(outbound[0].body.contains("Q3 is up 12%."));
    }

    /// **The security boundary.** With no `email` grant the send is REFUSED
    /// outright — before the mailbox, before the thread check — and nothing
    /// leaves the process.
    #[tokio::test]
    async fn email_without_the_grant_is_denied_and_nothing_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        // Established thread AND a wired mailbox: the ONLY thing missing is the
        // grant, so a pass here could only come from the grant check.
        h.receive_from("ada@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["docs.*", "web"]),
            &graph("email", Some("ada@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Denied);
        assert!(reports[0].detail.contains("[tools].allow"), "{reports:?}");
        assert!(h.mail.sent().is_empty(), "a denied send must not go out");
        assert!(
            h.inbox_messages().await.iter().all(|m| !m.outbound),
            "a denied send must leave no outbound record"
        );
    }

    /// **The security boundary, second gate.** Granted but COLD: the company's
    /// inbox holds nothing from this address, so the workflow may not open the
    /// conversation. Skipped and reported — never sent.
    #[tokio::test]
    async fn email_to_a_cold_recipient_is_skipped_and_nothing_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        // A different address wrote in; the target never did.
        h.receive_from("someone-else@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Skipped);
        assert!(reports[0].detail.contains("never written"), "{reports:?}");
        assert!(
            h.mail.sent().is_empty(),
            "a cold recipient must not be mailed"
        );
    }

    // --- email: cold recipients park (issue #227) ----------------------------

    /// **Issue #227, the headline.** Cold and granted, with an approvals queue
    /// wired: the report is PARKED rather than dropped. One `pending` row,
    /// nothing mailed, and a real card in the journal the operator's
    /// `/approvals` list reads.
    ///
    /// Note the policy mode: `full`. That is deliberately the mode under which
    /// `ApprovalGate::evaluate` returns `Allow` for a `Send` effect, so if this
    /// path ever grew an evaluate-then-dispatch step the mail would go out here
    /// and this test would fail on `sent()`.
    #[tokio::test]
    async fn a_cold_recipient_is_parked_for_approval_and_nothing_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true).with_parking(dir.path(), "full");
        h.receive_from("someone-else@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Pending, "{reports:?}");
        assert_eq!(
            reports[0].target.as_deref(),
            Some("stranger@example.com"),
            "{reports:?}"
        );
        // The row has to point somewhere, or `pending` is just a nicer word for
        // dropped.
        assert!(reports[0].detail.contains("Approvals"), "{reports:?}");
        // THE INVARIANT: a cold recipient never auto-sends.
        assert!(
            h.mail.sent().is_empty(),
            "a cold recipient must not be mailed, parked or not"
        );
        assert!(
            h.inbox_messages().await.iter().all(|m| !m.outbound),
            "nothing was sent, so there is no outbound record to leave"
        );

        // And the card really is in the durable queue — this is what
        // `/approvals` lists and what boot replay rehydrates.
        let pending = h.journal.as_ref().unwrap().pending();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].effect.kind, EMAIL_SEND_KIND);
    }

    /// The parked effect must have the **same shape as the agent path's**
    /// (`CycleHostImpl::send_email`), field for field. Not cosmetic: the
    /// operator sees one kind of card either way, and `perform_effect` keys on
    /// `kind` plus the `to`/`subject`/`body` payload to actually mail it on
    /// approval. A drift here parks cards that do nothing when approved.
    #[tokio::test]
    async fn the_parked_effect_matches_the_agent_paths_shape() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true).with_parking(dir.path(), "full");

        deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        let pending = h.journal.as_ref().unwrap().pending();
        let effect = &pending[0].effect;
        assert_eq!(effect.kind, EMAIL_SEND_KIND, "same kind constant");
        assert_eq!(effect.group, EffectGroup::Send);
        assert_eq!(effect.amount_usd, None, "a send costs nothing to gate on");
        // The two flags that say *why* this parked: cold counterparty.
        assert!(!effect.established_thread);
        assert!(effect.first_time_counterparty);
        // The payload `perform_effect` reads.
        assert_eq!(effect.payload["to"], "stranger@example.com");
        assert!(
            effect.payload["subject"].as_str().unwrap().contains("Acme"),
            "{effect:?}"
        );
        assert!(
            effect.payload["body"]
                .as_str()
                .unwrap()
                .contains("Q3 is up 12%."),
            "the report itself is the body, or approving sends an empty mail: {effect:?}"
        );
        // The gate holds the identical effect under the same id, so resolving
        // the approval returns something executable.
        let parked = h
            .gate
            .as_ref()
            .unwrap()
            .parked_effect(&pending[0].id)
            .expect("the gate holds the same id the journal recorded");
        assert_eq!(parked.payload, effect.payload);
        assert_eq!(parked.kind, effect.kind);
    }

    /// **Data integrity (PR #256 review).** A journal write that fails AFTER the
    /// gate accepted the park must leave **no gate entry behind**.
    ///
    /// The half-wired state the bundled [`DeliveryParking`] makes unrepresentable
    /// is a *construction* mistake; this is the *runtime* version of it, and
    /// bundling does nothing for it. An orphaned gate entry is the worst of the
    /// three outcomes: an executable effect sitting in the queue with no durable
    /// record, visible now and gone on the next restart, backing a row that
    /// promises a card which will not survive.
    ///
    /// Asserts all four halves of the rollback: `skipped` (not `pending`), no
    /// gate entry, no in-memory queue entry, and nothing sent.
    #[tokio::test]
    async fn a_failed_journal_write_leaves_no_orphaned_gate_entry() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true).with_failing_journal(dir.path(), "full");
        h.receive_from("someone-else@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        // Degrades to the pre-#227 row, never `pending`: there is no durable
        // card to point the operator at.
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(
            reports[0].status,
            DeliveryStatus::Skipped,
            "a park that could not be journaled is not pending: {reports:?}"
        );
        assert!(
            reports[0].detail.contains("could not be queued"),
            "{reports:?}"
        );

        // THE FINDING: the gate must not still hold the effect.
        assert!(
            h.gate.as_ref().unwrap().parked_ids().is_empty(),
            "a journal write failure must retract the gate entry, not orphan it"
        );
        // …and the operator's queue must not list a card the gate can no longer
        // execute. `record_parked` inserts before it appends, so this only holds
        // because the rollback clears it too.
        assert!(
            h.journal.as_ref().unwrap().pending().is_empty(),
            "no phantom card may be left in the approvals queue"
        );
        // The refusal still held throughout.
        assert!(h.mail.sent().is_empty(), "nothing may leave the process");
    }

    /// **Fail-closed (issue #227).** With no approvals queue wired, delivery
    /// degrades to the pre-#227 `skipped` row rather than promising a `pending`
    /// card that nothing is backing. A `pending` row on a runtime with no queue
    /// would send the operator to an empty Approvals list.
    #[tokio::test]
    async fn a_cold_recipient_without_a_queue_falls_back_to_skipped() {
        let dir = tempfile::tempdir().unwrap();
        // No `with_parking`: exactly the shape every non-production
        // construction site builds.
        let h = Harness::new(dir.path(), true, true);
        assert!(h.deps.parking.is_none());

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(
            reports[0].status,
            DeliveryStatus::Skipped,
            "never `pending` with no queue to back it: {reports:?}"
        );
        assert!(reports[0].detail.contains("never written"), "{reports:?}");
        assert!(h.mail.sent().is_empty());
    }

    /// The established-thread gate is unchanged by #227: a recipient who DID
    /// write in still sends immediately, and parks nothing. Parking is what
    /// happens to the refusal, not a new hurdle in front of a legitimate send.
    #[tokio::test]
    async fn an_established_recipient_still_sends_without_parking() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true).with_parking(dir.path(), "full");
        h.receive_from("ada@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("ada@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
        assert_eq!(h.mail.sent().len(), 1);
        assert!(
            h.journal.as_ref().unwrap().pending().is_empty(),
            "an established send must not clutter the approvals queue"
        );
    }

    /// The grant gate is unchanged by #227 too, and still runs FIRST: an
    /// ungranted company's cold send is `denied` outright, never parked. Parking
    /// an effect the company has no grant for would put a card in front of the
    /// operator that policy already refused — approving it would be an end-run
    /// around `[tools].allow`.
    #[tokio::test]
    async fn an_ungranted_cold_recipient_is_denied_not_parked() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true).with_parking(dir.path(), "full");

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["docs.*"]),
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports[0].status, DeliveryStatus::Denied, "{reports:?}");
        assert!(
            h.journal.as_ref().unwrap().pending().is_empty(),
            "a denied destination must not reach the approvals queue"
        );
        assert!(h.mail.sent().is_empty());
    }

    /// A company with no mailbox is still `skipped`, not parked: there is
    /// nothing to send from, so approving a card would fail at the transport.
    /// That arm is checked before the thread gate and #227 does not move it.
    #[tokio::test]
    async fn a_company_without_a_mailbox_is_skipped_not_parked() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, true).with_parking(dir.path(), "full");

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports[0].status, DeliveryStatus::Skipped, "{reports:?}");
        assert!(reports[0].detail.contains("no mailbox"), "{reports:?}");
        assert!(h.journal.as_ref().unwrap().pending().is_empty());
    }

    /// **Regression (PR #226 review).** A busy company's inbox must not lose an
    /// established recipient. `InboxStore::messages` returns oldest-first, so a
    /// capped read takes the OLDEST page — and an inbox that outgrows the cap
    /// silently stops finding anyone whose mail arrived after it. The failure
    /// is fail-closed (never a wrong send) but it is still wrong, and it bites
    /// exactly the longest-lived tenants.
    ///
    /// Note the direction: the sender's message must be buried *past* the cap,
    /// i.e. among the NEWEST mail. A sender whose message is the oldest sits at
    /// index 0 and was always found, cap or no cap.
    #[tokio::test]
    async fn an_established_sender_is_found_past_the_old_scan_cap() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        // 600 older messages from other people fill the first page…
        for i in 0..600 {
            h.receive_from(&format!("filler{i}@example.com")).await;
        }
        // …so the real correspondent's mail lands well past a 500-message cap.
        h.receive_from("ada@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("ada@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(
            reports[0].status,
            DeliveryStatus::Sent,
            "a correspondent buried past the scan cap is still an established \
             thread: {reports:?}"
        );
        assert_eq!(h.mail.sent().len(), 1);
    }

    /// **The default-configuration case (after #230).** A company with no
    /// `[tools]` section at all now defaults to the globals `default_allow`,
    /// and `*` satisfies the `email` grant — so on the majority of tenants the
    /// grant gate is open and the established-thread gate is the one actually
    /// holding the line. Pin that it does: a default-configured company still
    /// cannot cold-email a stranger.
    #[tokio::test]
    async fn a_default_configured_company_still_cannot_cold_email() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"
"#,
        )
        .expect("valid manifest");
        let mut rec = record(&[]);
        rec.manifest = manifest;
        // Sanity: the default really does grant `email` — if this ever stops
        // being true the test below would pass for the wrong reason.
        assert!(
            crate::harness::build::grants_cover(&rec.manifest.tools.allow, "email"),
            "expected the post-#230 default belt to cover `email`, got {:?}",
            rec.manifest.tools.allow
        );

        let reports = deliver_outputs(
            Some(&h.deps),
            &rec,
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(
            reports[0].status,
            DeliveryStatus::Skipped,
            "the established-thread gate must still refuse a stranger: {reports:?}"
        );
        assert!(h.mail.sent().is_empty(), "nothing may leave the process");
    }

    /// The company's OWN prior outbound mail to an address does not make that
    /// address established — otherwise one send would bootstrap the next.
    #[tokio::test]
    async fn a_prior_outbound_does_not_establish_a_thread() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        h.inbox
            .append(
                &h.company,
                &EmailRecord {
                    id: generate_id(),
                    inbox: local_part(COMPANY_ADDRESS),
                    from_name: String::new(),
                    from_email: "stranger@example.com".to_string(),
                    subject: "earlier".to_string(),
                    body: "earlier".to_string(),
                    at_millis: 1,
                    read: true,
                    outbound: true,
                },
            )
            .await
            .unwrap();

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports[0].status, DeliveryStatus::Skipped);
        assert!(h.mail.sent().is_empty());
    }

    /// Granted and established, but the company has no mailbox: skipped, with a
    /// reason distinct from the cold-recipient one.
    #[tokio::test]
    async fn email_without_a_mailbox_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, true);
        h.receive_from("ada@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("ada@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports[0].status, DeliveryStatus::Skipped);
        assert!(reports[0].detail.contains("no mailbox"), "{reports:?}");
    }

    /// A transport refusal is reported as `failed` — and, critically,
    /// `deliver_outputs` still returns normally, because the run's work is done
    /// and must not be thrown away over a mail hiccup.
    #[tokio::test]
    async fn a_send_failure_is_reported_and_does_not_abort_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = Harness::new(dir.path(), true, true);
        h.deps.mail = Some(CompanyMail {
            sender: Arc::new(RefusingMailSender),
            smtp: smtp_creds(),
        });
        h.receive_from("ada@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("ada@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Failed);
        assert!(reports[0].detail.contains("smtp said no"), "{reports:?}");
        assert_eq!(reports[0].reason, DeliveryReason::MailTransportRefused);
        // A refused send leaves no outbound audit record — the mail never went.
        assert!(h.inbox_messages().await.iter().all(|m| !m.outbound));
    }

    /// **Issue #248 at the source.** A real SMTP refusal quotes the mailbox it
    /// refused, so the transport's own words are an address-bearing string. This
    /// asserts the split holds where the row is built: `detail` keeps the reply
    /// (the operator needs it), `reason` cannot carry it.
    ///
    /// `.invalid` is reserved by RFC 2606 and can never resolve, so the fixture
    /// names nobody even if it escapes.
    #[tokio::test]
    async fn a_refusal_that_quotes_the_address_keeps_it_out_of_the_loggable_half() {
        const ADDRESS: &str = "recipient@example.invalid";

        /// Refuses the way a real MTA does: `550` with the rejected mailbox
        /// echoed back inside the reply.
        struct AddressQuotingMailSender;

        #[async_trait]
        impl MailSender for AddressQuotingMailSender {
            async fn send(
                &self,
                _creds: &MailCredentials,
                email: &OutboundEmail,
            ) -> Result<(), OpenCompanyError> {
                Err(OpenCompanyError::Config(format!(
                    "550 5.1.1 <{}>: Recipient address rejected: User unknown in local recipient \
                     table",
                    email.to
                )))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut h = Harness::new(dir.path(), true, true);
        h.deps.mail = Some(CompanyMail {
            sender: Arc::new(AddressQuotingMailSender),
            smtp: smtp_creds(),
        });
        h.receive_from(ADDRESS).await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some(ADDRESS)),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        let row = &reports[0];
        assert_eq!(row.status, DeliveryStatus::Failed);

        // The operator's half is untouched: the reply is what makes this
        // fixable, and the run response goes to the tenant, not the platform.
        assert!(row.detail.contains(ADDRESS), "{row:?}");
        assert!(row.detail.contains("550 5.1.1"), "{row:?}");

        // The loggable half classifies the same failure and cannot carry the
        // address — not by scrubbing it, but by having nowhere to put it.
        assert_eq!(row.reason, DeliveryReason::MailTransportRefused);
        let reason = row.reason.to_string();
        assert!(!reason.contains(ADDRESS), "{reason}");
        assert!(!reason.contains('@'), "{reason}");
        assert!(
            reason.contains("the mail transport refused the message"),
            "{reason}"
        );
    }

    // --- channel -------------------------------------------------------------

    #[tokio::test]
    async fn channel_posts_to_the_wired_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = Harness::new(dir.path(), true, true);
        h.deps.channels = vec![Arc::new(DeskChannel::new(
            h.company.clone(),
            "engineering".to_string(),
            h.events.clone(),
        ))];

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("channel", Some("engineering")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent);
        let events = h
            .events
            .read_from(&h.company, crate::ports::types::EventSeq::new(0), 20)
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.event,
            CompanyEvent::AgentReply { chat_id, text, .. }
                if chat_id == "engineering" && text.contains("Q3 is up 12%.")
        )));
    }

    /// A channel the deployment never wired cannot be conjured by a graph. The
    /// failure names what IS wired, so the fix is obvious from the run result.
    #[tokio::test]
    async fn channel_that_is_not_wired_fails_with_the_wired_list() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("channel", Some("telegram")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Failed);
        // The unwired failure now speaks in the same sentence the console's
        // picker pre-flight shows (issue #981), naming what IS wired — which,
        // since #1757, includes the durable `operator` channel this Harness wires.
        assert!(
            reports[0]
                .detail
                .contains("is not a workflow delivery channel"),
            "{reports:?}"
        );
        assert!(reports[0].detail.contains(OPERATOR_CHANNEL), "{reports:?}");
        // The two channel failures are classified apart: "you named a channel
        // that does not exist" and "the channel said no" want different fixes,
        // and the log line only ever sees this half.
        assert_eq!(reports[0].reason, DeliveryReason::ChannelNotWired);
        // The channel id — which for this arm IS the target — stays off the
        // loggable half, same rule as a recipient address (issue #248).
        assert!(
            !reports[0].reason.to_string().contains("telegram"),
            "{reports:?}"
        );
        assert!(h.channel.sent().is_empty());
    }

    // --- reachability & wiring ----------------------------------------------

    /// An `output` node on a branch the run never took gets no attempt and NO
    /// ROW. An absent row means "not reached", never "silently dropped".
    #[tokio::test]
    async fn an_unreached_output_node_produces_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        h.add_admin("u1", "ada@acme.test").await;
        // The engine reached `start` but never `done`.
        let output = serde_json::json!({
            "nodes": { "start": { "items": [{ "json": { "seed": 1 } }] } }
        });

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("owner", None),
            "run-1",
            &output,
            &[],
        )
        .await;

        assert!(reports.is_empty(), "{reports:?}");
        assert!(h.mail.sent().is_empty());
        assert!(h.channel.sent().is_empty());
    }

    /// An `output` node with no `destination` is the pre-#170 shape. It still
    /// shows in the run drawer, still sends nothing, and — since #925 — says so
    /// with a `Skipped` row instead of contributing nothing at all.
    ///
    /// **This assertion is inverted from what it was.** It previously read
    /// `reports.is_empty()`, which is the behaviour #925 was filed against:
    /// silence made "the author routed nothing on purpose" and "the author never
    /// configured a destination" the same observation. The transport assertions
    /// below are the part that must not change — nothing is sent either way.
    #[tokio::test]
    async fn an_output_node_without_a_destination_reports_the_gap_and_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        let plain = parse_workflow(
            r#"
id = "plain"
name = "Plain"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#,
        )
        .expect("parses");

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &plain,
            "run-1",
            &reached_output(),
            &[],
        )
        .await;
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Skipped);
        assert_eq!(reports[0].reason, DeliveryReason::NoDestinationConfigured);
        assert!(
            h.mail.sent().is_empty() && h.channel.sent().is_empty(),
            "the row is a statement about configuration; nothing may leave the process"
        );
    }

    /// The #169 lesson: an unwired delivery bundle must be LOUD. It writes a
    /// `failed` row onto the run result — where an operator actually looks —
    /// rather than skipping in a debug log.
    #[tokio::test]
    async fn unwired_delivery_reports_loudly_instead_of_skipping() {
        let reports = deliver_outputs(
            None,
            &record(&["*"]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Failed);
        assert_eq!(reports[0].node, "done");
        assert_eq!(reports[0].kind, "owner");
        assert!(reports[0].detail.contains("not wired"), "{reports:?}");
        assert!(
            reports[0].detail.contains("nothing was sent"),
            "{reports:?}"
        );
    }

    // --- issue #438: one delivery per approval lineage ------------------------

    /// One ledger row naming `node`, as a continuation's trigger input carries.
    fn already(node: &str, kind: &str) -> Vec<DeliveredReport> {
        vec![DeliveredReport {
            node: node.to_string(),
            kind: kind.to_string(),
        }]
    }

    /// **The regression.** A continuation reaches the same `output` node again,
    /// and must not mail the report a second time. The row says so, and the
    /// transport is never touched.
    #[tokio::test]
    async fn a_report_this_lineage_already_sent_is_not_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        h.add_admin("u1", "ada@acme.test").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &already("done", "owner"),
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Skipped);
        assert_eq!(reports[0].reason, DeliveryReason::AlreadyDelivered);
        assert_eq!(reports[0].node, "done");
        assert!(
            h.mail.sent().is_empty(),
            "the transport must never be reached: {:?}",
            h.mail.sent()
        );
        assert!(h.channel.sent().is_empty());
    }

    /// A report the first run **parked** counts as delivered too. Otherwise
    /// every continuation stacks a second identical cold-send card, and
    /// approving both mails the stranger twice — `park_cold_recipient` has no
    /// dedupe of its own.
    #[tokio::test]
    async fn a_report_this_lineage_already_parked_is_not_parked_again() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, false).with_parking(dir.path(), "full");
        // Cold: the company has never heard from this address.
        let cold = graph("email", Some("stranger@example.test"));

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["email"]),
            &cold,
            "run-1",
            &reached_output(),
            &already("done", "email"),
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Skipped);
        assert_eq!(reports[0].reason, DeliveryReason::AlreadyDelivered);
        assert!(
            h.journal
                .as_ref()
                .expect("parking wired")
                .pending()
                .is_empty(),
            "a continuation must not stack a second card for one send"
        );
        assert!(h.mail.sent().is_empty());
    }

    /// The ledger suppresses the node it names and nothing else. A second
    /// output node in the same graph still delivers — otherwise one earlier
    /// send would silence the whole graph.
    #[tokio::test]
    async fn a_node_the_ledger_does_not_name_still_delivers() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        h.add_admin("u1", "ada@acme.test").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &already("some_other_node", "owner"),
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent);
        assert_eq!(h.mail.sent().len(), 1);
    }

    /// A run nobody resumed carries an empty ledger, and behaves exactly as it
    /// did before #438. Every other test in this module passes `&[]`, so this
    /// states the invariant they all rely on.
    #[tokio::test]
    async fn a_run_with_no_ledger_delivers_normally() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        h.add_admin("u1", "ada@acme.test").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent);
        assert_eq!(h.mail.sent().len(), 1);
    }

    /// An unreached node on the ledger produces no row at all: "not reached"
    /// outranks "already delivered", because there was nothing to deliver this
    /// time either way.
    #[tokio::test]
    async fn an_unreached_node_on_the_ledger_still_produces_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);
        let unreached = serde_json::json!({ "nodes": { "start": { "items": [] } } });

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            "run-1",
            &unreached,
            &already("done", "owner"),
        )
        .await;

        assert!(reports.is_empty(), "{reports:?}");
    }

    // --- report extraction ---------------------------------------------------

    /// Several items concatenate in order; the doubly-wrapped `json.json.text`
    /// the engine sometimes emits is read too, with the outer value winning.
    #[test]
    fn report_text_reads_plain_and_doubly_wrapped_items() {
        let output = serde_json::json!({
            "nodes": { "done": { "items": [
                { "json": { "text": "first" } },
                { "json": { "json": { "text": "second" } } },
                { "json": { "text": "outer", "json": { "text": "inner" } } },
            ] } }
        });
        assert_eq!(report_text(&output, "done"), "first\n\nsecond\n\nouter");
    }

    /// A data-shaped item with no `text` is delivered as JSON rather than
    /// dropped — an empty report would be worse than an ugly one.
    #[test]
    fn report_text_falls_back_to_json_for_a_textless_item() {
        let output = serde_json::json!({
            "nodes": { "done": { "items": [{ "json": { "revenue": 12 } }] } }
        });
        assert!(report_text(&output, "done").contains("revenue"));
    }

    #[test]
    fn report_text_of_a_node_with_no_items_says_so() {
        let output = serde_json::json!({ "nodes": { "done": { "items": [] } } });
        assert!(report_text(&output, "done").contains("no output"));
    }

    /// Truncation is character-indexed: a byte slice here would panic
    /// mid-codepoint on any multi-byte report.
    #[test]
    fn truncation_never_splits_a_codepoint() {
        let text = "é".repeat(50);
        let cut = truncate_chars(&text, 10);
        assert!(cut.starts_with(&"é".repeat(10)));
        assert!(cut.ends_with(TRUNCATION_MARKER));
        // Untouched when it fits.
        assert_eq!(truncate_chars("short", 10), "short");
    }

    // --- issue #529: the write-behind delivery record ------------------------

    /// A `Sent` dispatch journals exactly one `WorkflowReportDelivered`, shaped
    /// from the row — the durable record a crashed run leaves so a re-run can
    /// skip it.
    #[tokio::test]
    async fn a_sent_delivery_journals_one_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = Harness::new(dir.path(), false, true);
        h.deps.channels = vec![Arc::new(DeskChannel::new(
            h.company.clone(),
            "engineering".to_string(),
            h.events.clone(),
        ))];

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("channel", Some("engineering")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;
        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");

        let journaled = h.journaled_deliveries().await;
        assert_eq!(
            journaled.len(),
            1,
            "exactly one record per dispatch: {journaled:?}"
        );
        let CompanyEvent::WorkflowReportDelivered {
            workflow_id,
            run_id,
            node,
            kind,
            target,
        } = &journaled[0]
        else {
            panic!("expected a WorkflowReportDelivered, got {:?}", journaled[0]);
        };
        assert_eq!(workflow_id, "report_flow");
        assert_eq!(run_id, "run-1");
        assert_eq!(node, "done");
        assert_eq!(kind, "channel");
        assert_eq!(target.as_deref(), Some("engineering"));
        let events = h
            .events
            .read_from(&h.company, crate::ports::types::EventSeq::new(0), 20)
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.event,
            CompanyEvent::AgentReply { chat_id, text, .. }
                if chat_id == "engineering" && text.contains("Q3 is up 12%.")
        )));
    }

    /// A `Pending` park journals a record too: the card is durable and approving
    /// it sends, so a re-run must treat the report as already delivered —
    /// exactly as issue #438's in-lineage ledger counts a `Pending` row.
    #[tokio::test]
    async fn a_pending_park_journals_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true).with_parking(dir.path(), "full");
        h.receive_from("someone-else@example.com").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&["*"]),
            &graph("email", Some("stranger@example.com")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;
        assert_eq!(reports[0].status, DeliveryStatus::Pending, "{reports:?}");

        let journaled = h.journaled_deliveries().await;
        assert_eq!(
            journaled.len(),
            1,
            "a park is a delivery for the ledger: {journaled:?}"
        );
        let CompanyEvent::WorkflowReportDelivered { node, kind, .. } = &journaled[0] else {
            panic!("expected a WorkflowReportDelivered");
        };
        assert_eq!(node, "done");
        assert_eq!(kind, "email");
    }

    /// A row that did NOT leave the process journals nothing. A `Failed` channel
    /// (unwired target) leaves no record, so a re-run is free to retry it.
    #[tokio::test]
    async fn a_failed_delivery_journals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // No channel wired, so a `channel` destination fails.
        let h = Harness::new(dir.path(), false, false);

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("channel", Some("operator")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;
        assert_eq!(reports[0].status, DeliveryStatus::Failed, "{reports:?}");
        assert!(
            h.journaled_deliveries().await.is_empty(),
            "a failed dispatch left the process nothing to record"
        );
    }

    /// An `AlreadyDelivered` skip journals nothing — the report is on the ledger
    /// precisely because it already went out, so recording it again would double
    /// the very thing the ledger exists to prevent.
    #[tokio::test]
    async fn an_already_delivered_skip_journals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, true);

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("channel", Some("operator")),
            "run-1",
            &reached_output(),
            &[DeliveredReport {
                node: "done".to_string(),
                kind: "channel".to_string(),
            }],
        )
        .await;
        assert_eq!(reports[0].status, DeliveryStatus::Skipped, "{reports:?}");
        assert_eq!(reports[0].reason, DeliveryReason::AlreadyDelivered);
        assert!(
            h.journaled_deliveries().await.is_empty(),
            "a report skipped because it already went out is not re-recorded"
        );
    }

    /// A journal that cannot be written does not fail the delivery: the report
    /// still sends and its row is still `Sent`. Losing the record risks one
    /// duplicate on a later re-run — the accepted write-behind cost, never a
    /// failed send.
    #[tokio::test]
    async fn a_journal_failure_does_not_fail_a_delivery() {
        let dir = tempfile::tempdir().unwrap();
        // A channel that accepts the send, so the journal write is what this
        // case actually reaches. Pointed at `operator` it would fail on the
        // refusal instead and pass for the wrong reason.
        let h = Harness::new(dir.path(), false, true)
            .with_recording_channel("engineering")
            .with_failing_events();

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("channel", Some("engineering")),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
        assert_eq!(
            h.recording().sent().len(),
            1,
            "the report reached the channel despite the journal"
        );
    }

    /// Issue #542: the dry router runs the routing half only. A reached output
    /// node yields one `Skipped`/`DryRun` row naming where the report WOULD have
    /// gone — no deps, no transport, no journal.
    #[test]
    fn deliver_outputs_dry_routes_a_reached_node_without_sending() {
        let workflow = graph("email", Some("ada@example.com"));
        let reports = deliver_outputs_dry(&record(&["email"]), &workflow, &reached_output());
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].node, "done");
        assert_eq!(reports[0].status, DeliveryStatus::Skipped);
        assert_eq!(reports[0].reason, DeliveryReason::DryRun);
        assert_eq!(reports[0].target.as_deref(), Some("ada@example.com"));
        assert!(
            reports[0].detail.contains("email ada@example.com"),
            "the row should name where it would have gone: {}",
            reports[0].detail
        );
    }

    // --- issue #925: an unconfigured destination is not the same as no report --

    /// **The regression.** A run that reaches an output node naming no
    /// destination used to return an empty `deliveries` list, which the console
    /// renders as `Finished — this run routed no reports.` — the same sentence
    /// it shows a workflow that deliberately routed nothing. The row is what
    /// tells the two apart, and it carries the reason as a closed token so a
    /// reader does not have to parse prose.
    ///
    /// Deps are `None` here on purpose: the check must land *before* anything
    /// touches a transport, so this passes on a runtime with no delivery ports
    /// and would fail with a `NotWired` row if the arms were ever reordered.
    #[tokio::test]
    async fn a_reached_output_node_with_no_destination_says_so() {
        let reports = deliver_outputs(
            None,
            &record(&["*"]),
            &graph_without_destination(),
            "run-1",
            &reached_output(),
            &[],
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].node, "done");
        assert_eq!(reports[0].status, DeliveryStatus::Skipped);
        assert_eq!(reports[0].reason, DeliveryReason::NoDestinationConfigured);
        assert_eq!(
            reports[0].target, None,
            "there is no destination, so there is no target to name"
        );
        assert!(
            reports[0].detail.contains("no destination"),
            "the row has to say what is missing: {}",
            reports[0].detail
        );
    }

    /// The other half of the same rule: an output node with no destination that
    /// the run never reached still contributes nothing. "Never configured" is
    /// only worth reporting about a node the run actually arrived at — otherwise
    /// every untaken branch would file a complaint.
    #[tokio::test]
    async fn an_unreached_output_node_with_no_destination_stays_silent() {
        let output = serde_json::json!({ "nodes": { "start": { "items": [] } } });
        let reports = deliver_outputs(
            None,
            &record(&["*"]),
            &graph_without_destination(),
            "run-1",
            &output,
            &[],
        )
        .await;

        assert!(
            reports.is_empty(),
            "an unreached node owes no report either way: {reports:?}"
        );
    }

    /// An `output` node that only exists to pause for approval is control flow,
    /// not a report-back that lost its address. It contributes no row, so a
    /// correct gated workflow does not grow a "not delivered" badge on every
    /// continuation run.
    #[tokio::test]
    async fn an_approval_gate_with_no_destination_is_not_reported_as_misconfigured() {
        let gate = parse_workflow(
            r#"
id = "gated"
name = "Gated"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Gate"
requires_approval = true
[[edge]]
from = "start"
to = "done"
"#,
        )
        .expect("a gate graph is valid");
        let reports = deliver_outputs(
            None,
            &record(&["*"]),
            &gate,
            "run-1",
            &reached_output(),
            &[],
        )
        .await;
        assert!(
            reports.is_empty(),
            "a gate is not a report-back with a missing address: {reports:?}"
        );
    }

    /// A test run is where an author most wants to find this, so the dry router
    /// takes the same rule.
    #[test]
    fn deliver_outputs_dry_reports_a_node_with_no_destination() {
        let reports = deliver_outputs_dry(
            &record(&["email"]),
            &graph_without_destination(),
            &reached_output(),
        );
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].node, "done");
        assert_eq!(reports[0].status, DeliveryStatus::Skipped);
        assert_eq!(reports[0].reason, DeliveryReason::NoDestinationConfigured);
    }

    /// An output node the dry run never reached contributes no row at all —
    /// exactly the "absent means not reached" rule the live path takes.
    #[test]
    fn deliver_outputs_dry_skips_an_unreached_node() {
        let workflow = graph("owner", None);
        // Output where `done` was NOT reached.
        let output = serde_json::json!({ "nodes": { "start": { "items": [] } } });
        let reports = deliver_outputs_dry(&record(&["email"]), &workflow, &output);
        assert!(
            reports.is_empty(),
            "an unreached node routes nothing: {reports:?}"
        );
    }

    /// Issue #1825 (P1, fifth follow-up — found by chatgpt-codex-connector):
    /// "Prevent the synthetic hold from consuming a real decision".
    ///
    /// Pre-fix, `park_and_journal` called `self.approvals.park` — which is what
    /// makes an approval id exist for an operator to resolve — strictly
    /// *before* arming this card's own `ContinuationQueue` slot (that arm ran
    /// only after `record_parked` returned, on the success path). A resolve
    /// racing in on another tokio worker thread during `record_parked`'s own
    /// async durable append therefore saw a turn whose only armed slot was
    /// `park_gated_calls`'s pre-loop synthetic hold, decided against it, and
    /// released the batch before this card had been counted; this card's own
    /// arm then still landed once the journal write returned, into a fresh,
    /// orphaned queue entry no further decision would ever redeem.
    ///
    /// This spies on the approval gate `park_and_journal` calls first and
    /// captures `continuations.outstanding(turn)` at that exact point —
    /// deterministic, no wall-clock race needed, on the same principle as
    /// `approving_the_first_card_of_a_multi_call_node_does_not_complete_the_batch_early`
    /// in `workflows::caps::mod`. Pre-fix this captures `0` (nothing armed
    /// yet); post-fix it must capture `1`.
    #[tokio::test]
    async fn park_and_journal_arms_the_continuation_slot_before_the_card_is_parkable() {
        use crate::ports::types::PolicyDecision;

        /// Delegates every call to `inner`, except that `park` first records
        /// how many decisions `turn` is already counted as blocking on —
        /// the moment an operator's resolve could first reach this approval.
        struct Spy {
            inner: Arc<dyn ApprovalGate>,
            continuations: crate::runtime::continuation::ContinuationQueue,
            turn: String,
            outstanding_at_park: std::sync::Mutex<Option<usize>>,
        }

        #[async_trait]
        impl ApprovalGate for Spy {
            async fn evaluate(
                &self,
                company: &CompanyId,
                effect: &Effect,
            ) -> crate::Result<PolicyDecision> {
                self.inner.evaluate(company, effect).await
            }

            async fn park(&self, company: &CompanyId, effect: Effect) -> crate::Result<ApprovalId> {
                *self.outstanding_at_park.lock().expect("spy lock") =
                    Some(self.continuations.outstanding(&self.turn));
                self.inner.park(company, effect).await
            }

            async fn resolve(
                &self,
                id: &ApprovalId,
                verdict: Verdict,
                by: Actor,
            ) -> crate::Result<Option<Effect>> {
                self.inner.resolve(id, verdict, by).await
            }
        }

        let dir = tempfile::Builder::new()
            .prefix("oc-1825-p1-5-")
            .tempdir()
            .expect("tempdir");
        let h = Harness::new(dir.path(), false, false).with_parking(dir.path(), "full");
        let parking = h.deps.parking.clone().expect("with_parking wired it");

        let turn = "workflow-node:run-1825-p1-5:work".to_string();
        let spy = Arc::new(Spy {
            inner: parking.approvals.clone(),
            continuations: parking.continuations.clone(),
            turn: turn.clone(),
            outstanding_at_park: std::sync::Mutex::new(None),
        });
        let mut spied_parking = parking.clone();
        spied_parking.approvals = spy.clone();

        let effect = Effect {
            kind: "shell".to_string(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "call": "shell" }),
            agent: Some("ceo".to_string()),
            run_id: None,
        };

        let approval_id = spied_parking
            .park_and_journal(
                &CompanyId::new("acme"),
                effect,
                crate::runtime::journal::TaskLink::Unlinked,
                None,
                Some(turn.clone()),
            )
            .await
            .expect("parks");

        let captured = spy
            .outstanding_at_park
            .lock()
            .expect("spy lock")
            .expect("park was called");
        assert_eq!(
            captured, 1,
            "this card's continuation slot must already be armed by the time the approval \
             gate's park() runs, before record_parked's synchronous insert can make the card \
             resolvable to a concurrent operator — otherwise a decision racing in during \
             record_parked's async durable append can consume a hold this card was never \
             counted against"
        );

        // Sanity: the ordinary, non-racing shape is unchanged — one card on
        // this turn, one decision, releases it immediately.
        assert_eq!(parking.continuations.outstanding(&turn), 1);
        let event = CompanyEvent::ApprovalResolved {
            approval_id,
            verdict: Verdict::Approve,
            by: Actor {
                kind: ActorKind::Operator,
                id: "operator".to_string(),
            },
        };
        assert!(
            parking.continuations.decide(&turn, Some(event)).is_some(),
            "the only card parked on this turn must still release it on its own decision"
        );
    }

    /// Companion to the test above: when the durable journal write fails, the
    /// slot armed before the attempt must be released rather than left
    /// blocking the turn on a card that will now never exist.
    #[tokio::test]
    async fn park_and_journal_releases_the_continuation_slot_when_the_journal_write_fails() {
        let dir = tempfile::Builder::new()
            .prefix("oc-1825-p1-5-fail-")
            .tempdir()
            .expect("tempdir");
        let h = Harness::new(dir.path(), false, false).with_failing_journal(dir.path(), "full");
        let parking = h
            .deps
            .parking
            .clone()
            .expect("with_failing_journal wired it");

        let turn = "workflow-node:run-1825-p1-5-fail:work".to_string();
        let effect = Effect {
            kind: "shell".to_string(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "call": "shell" }),
            agent: Some("ceo".to_string()),
            run_id: None,
        };

        let result = parking
            .park_and_journal(
                &CompanyId::new("acme"),
                effect,
                crate::runtime::journal::TaskLink::Unlinked,
                None,
                Some(turn.clone()),
            )
            .await;
        assert!(
            result.is_err(),
            "the failing journal must still fail the park"
        );
        assert_eq!(
            parking.continuations.outstanding(&turn),
            0,
            "a park whose durable write failed leaves no card for an operator to ever decide, \
             so the slot armed for it before the attempt must be released — otherwise the turn \
             is left permanently blocked on a decision that can never arrive"
        );
    }
}
