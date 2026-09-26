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
//!   falls back to an operator notification plus the report in the DM of the
//!   agent responsible for the workflow ([`report_dm`]).
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
use crate::ports::notifications::{Notification, NotificationStore, Subject, SubjectKind};
use crate::ports::types::{
    ApprovalId, CompanyEvent, CompanyId, CompanyRecord, Effect, EffectGroup, OutboundMessage,
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
    /// present here — `RuntimeBuilder::build` drops it by identity. A report
    /// bound for the operator goes through [`report_to_operator`] instead.
    pub channels: Vec<Arc<dyn ChannelAdapter>>,
    /// Where an operator report files its notification. `None` skips the
    /// notification; the report still lands in the responsible agent's DM.
    pub notifications: Option<Arc<dyn NotificationStore>>,
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
    /// The company's live grants, so a park marks its work unit's checkout as
    /// held until the approval resolves.
    pub grants: crate::runtime::grants::GrantSet,
    /// The company's event journal, for the `ApprovalParked` nudge every
    /// console listens for.
    pub events: Arc<dyn EventLog>,
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
            workflow,
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
#[allow(clippy::too_many_arguments)]
async fn deliver_one(
    delivery: &WorkflowDeliveryDeps,
    record: &CompanyRecord,
    workflow: &WorkflowFile,
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
                // No mailbox, or no admin has an address: the report goes to
                // the operator as a notification and into the DM of the agent
                // responsible for the workflow.
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
                        match report_to_operator(delivery, record, workflow, subject, text, true)
                            .await
                        {
                            Ok(dm) => row(
                                Some(dm.clone()),
                                DeliveryStatus::Sent,
                                why_reason,
                                format!("{why}, so the report went to the operator and to {dm}"),
                            ),
                            Err((_class, detail)) => row(
                                None,
                                DeliveryStatus::Failed,
                                DeliveryReason::OwnerFallbackFailed,
                                format!("{why}, and the operator fallback failed: {detail}"),
                            ),
                        },
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
            if target == crate::runtime::channel::OPERATOR_CHANNEL {
                reports.push(
                    match report_to_operator(delivery, record, workflow, subject, text, false).await
                    {
                        Ok(dm) => row(
                            Some(target.to_string()),
                            DeliveryStatus::Sent,
                            DeliveryReason::ChannelPosted,
                            format!("reported to the operator and to {dm}"),
                        ),
                        Err((reason, detail)) => row(
                            Some(target.to_string()),
                            DeliveryStatus::Failed,
                            reason,
                            detail,
                        ),
                    },
                );
                return;
            }
            reports.push(
                match post_to_channel(delivery, target, subject, text).await {
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
    /// The shared park transaction over this bundle's handles.
    fn parker(&self) -> crate::runtime::approval_park::ApprovalParker {
        crate::runtime::approval_park::ApprovalParker::new(
            self.approvals.clone(),
            self.journal.clone(),
            self.grants.clone(),
            self.continuations.clone(),
            self.events.clone(),
        )
    }

    /// Parks `effect` on the gate and journals it — **both halves or neither**
    /// — through the shared [`ApprovalParker`](crate::runtime::approval_park::ApprovalParker),
    /// then arms the workflow gate queue for `turn`.
    ///
    /// The parker counts `turn`'s continuation slot before the approval is
    /// visible, retracts the gate entry if the journal write fails, marks the
    /// work unit pending on the company's grants, and emits `ApprovalParked`.
    ///
    /// `task_link` and `thread` are parameters because they are the two facts
    /// only the caller knows: which board card owns the request, and which
    /// conversation to raise it in.
    pub(crate) async fn park_and_journal(
        &self,
        company: &CompanyId,
        effect: Effect,
        task_link: crate::runtime::journal::TaskLink,
        thread: Option<String>,
        turn: Option<String>,
    ) -> Result<ApprovalId, crate::error::OpenCompanyError> {
        let approval_id = self
            .parker()
            .park(
                company,
                effect.clone(),
                crate::runtime::approval_park::ParkSite {
                    task: task_link,
                    // A workflow request is not raised by a chat message, so
                    // there is no thread root to hang a continuation under.
                    conversation: ApprovalConversation {
                        thread,
                        parent: None,
                    },
                    turn: turn.clone(),
                },
            )
            .await?;
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
    channel_id: &str,
    subject: &str,
    text: &str,
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
    adapter
        .send(OutboundMessage {
            message_id: None,
            task_id: None,
            outputs: Vec::new(),
            channel: channel_id.to_string(),
            agent: None,
            text: format!("{subject}\n\n{text}"),
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

/// The header prefixed to every report addressed to the operator.
const OPERATOR_REPORT_HEADER: &str = "Workflow report";

/// Formats a report for the operator with its source header, so it reads as a
/// report named by its workflow and node rather than as chat text.
fn operator_report(subject: &str, text: &str) -> String {
    format!("{OPERATOR_REPORT_HEADER} — {subject}\n\n{text}")
}

/// The DM a workflow's operator reports land in: that of the agent responsible
/// for the workflow — its owning desk's lead, else the orchestrator — resolved
/// by the same rule a parked blocker's sender is.
pub(crate) fn report_dm(record: &CompanyRecord, workflow: &WorkflowFile) -> String {
    let sender = crate::company::blocker_sender::resolve_sender(
        record,
        &crate::company::blocker_sender::BlockerSenderSignals {
            started_by: None,
            owner_desk: workflow.owner_desk.clone(),
            assignee: None,
        },
    );
    crate::company::blocker_sender::dm_thread(&sender)
}

/// Reports to the operator: the report is journaled into the responsible
/// agent's DM ([`report_dm`]) and a notification pointing at it is filed.
/// Returns the DM it landed in.
///
/// `admin_only` marks an `owner` fallback: the journaled row is authored as
/// [`OWNER_FALLBACK_REPORT_AUTHOR`](crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR)
/// so the read path shows it to administrators only, and the notification is
/// addressed to the company's active admins — the audience the email branch
/// would have reached.
async fn report_to_operator(
    delivery: &WorkflowDeliveryDeps,
    record: &CompanyRecord,
    workflow: &WorkflowFile,
    subject: &str,
    text: &str,
    admin_only: bool,
) -> Result<String, (DeliveryReason, String)> {
    let dm = report_dm(record, workflow);
    let author = if admin_only {
        crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR
    } else {
        crate::runtime::channel::WORKFLOW_REPLY_AUTHOR
    };
    tracing::debug!(
        company = %record.id,
        workflow = %workflow.id,
        dm = %dm,
        admin_only,
        "[workflow-delivery] reporting to the operator"
    );
    delivery
        .events
        .append(
            &record.id,
            CompanyEvent::AgentReply {
                audience: Vec::new(),
                episode: None,
                chat_id: dm.clone(),
                agent_id: author.to_string(),
                text: operator_report(subject, text),
                steps: Vec::new(),
                task_id: None,
                outputs: Vec::new(),
                parent: None,
                mentions: Vec::new(),
                mention_depth: 0,
            },
        )
        .await
        .map_err(|err| {
            (
                DeliveryReason::ChannelRefused,
                format!("the report could not be journaled to {dm}: {err}"),
            )
        })?;
    if let Some(notifications) = &delivery.notifications {
        let audience = if admin_only {
            active_admin_ids(delivery.users.as_ref(), &record.id).await
        } else {
            None
        };
        let note = Notification {
            id: generate_id(),
            kind: "workflow_report".to_string(),
            subject: Subject {
                kind: SubjectKind::Workflow,
                id: workflow.id.clone(),
            },
            created_at: now_millis(),
            title: format!(
                "{OPERATOR_REPORT_HEADER} — {}",
                subject.replace(['\r', '\n'], " ")
            ),
            audience,
            context: Some(dm.clone()),
        };
        if let Err(err) = notifications.append(&record.id, &note).await {
            tracing::warn!(
                company = %record.id,
                workflow = %workflow.id,
                error = %err,
                "[workflow-delivery] the report landed but its notification could not be recorded"
            );
        }
    }
    Ok(dm)
}

/// The ids of the company's active admins, as a notification audience. `None`
/// when there are none or the directory cannot be read, which addresses the
/// whole company rather than nobody.
async fn active_admin_ids(users: &dyn UserStore, company: &CompanyId) -> Option<Vec<String>> {
    let ids: Vec<String> = users
        .list_users(company)
        .await
        .map_err(|err| {
            tracing::warn!(
                company = %company,
                error = %err,
                "[workflow-delivery] could not list admins for a report notification"
            );
        })
        .ok()?
        .into_iter()
        .filter(|user| user.role == UserRole::Admin && user.status == UserStatus::Active)
        .map(|user| user.id)
        .collect();
    (!ids.is_empty()).then_some(ids)
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
#[path = "delivery_channel_grants_tests.rs"]
mod tests_channel_grants;
#[cfg(test)]
#[path = "delivery_dry_and_park_tests.rs"]
mod tests_dry_and_park;
#[cfg(test)]
#[path = "delivery_owner_routing_tests.rs"]
mod tests_owner_routing;
#[cfg(test)]
#[path = "delivery_owner_setup_tests.rs"]
mod tests_owner_setup;
#[cfg(test)]
#[path = "delivery_unwired_dedup_tests.rs"]
mod tests_unwired_dedup;
