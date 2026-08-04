//! Route an `output` node's report to a person or a channel (issue #170).
//!
//! An `output` node is a workflow's terminal "report back". Before this module
//! existed it produced a value that surfaced only in the console's transient
//! run-result drawer — so a workflow could compute an owner summary and had no
//! way to send it anywhere. A node's
//! [`destination`](crate::company::WorkflowDestinationDef) closes that gap.
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
//!   falls back to the always-present `operator` channel rather than becoming a
//!   silent no-op.
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
    Actor, ActorKind, CompanyId, CompanyRecord, Effect, EffectGroup, OutboundMessage, Verdict,
};
use crate::ports::{
    ApprovalGate, ChannelAdapter, DeliveryReason, DeliveryReport, DeliveryStatus, EmailRecord,
    InboxStore, UserRole, UserStatus, UserStore, generate_id, now_millis,
};
use crate::runtime::cycle::EMAIL_SEND_KIND;
use crate::runtime::journal::RuntimeJournal;
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
    /// `None` when the company has no mailbox: `owner` then falls back to the
    /// operator channel, and `email` is reported `skipped`.
    pub mail: Option<CompanyMail>,
    /// The company's inboxes — both the established-thread check and the
    /// outbound audit record go through this port.
    pub inbox: Arc<dyn InboxStore>,
    /// The company's user directory: how an `owner` destination resolves to
    /// actual addresses, server-side.
    pub users: Arc<dyn UserStore>,
    /// Every wired channel adapter, including the always-present `operator`.
    pub channels: Vec<Arc<dyn ChannelAdapter>>,
    /// What a cold `email` recipient is parked on (issue #227). `None` fails
    /// closed to the pre-#227 behaviour: the report is `skipped`, never a
    /// `pending` row no queue is backing.
    pub parking: Option<DeliveryParking>,
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
}

impl std::fmt::Debug for WorkflowDeliveryDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the mail handle itself — `CompanyMail` carries `SmtpCredentials`,
        // whose derived `Debug` prints the password (see `mailer::test`).
        f.debug_struct("WorkflowDeliveryDeps")
            .field("mail", &self.mail.is_some())
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
pub async fn deliver_outputs(
    delivery: Option<&WorkflowDeliveryDeps>,
    record: &CompanyRecord,
    workflow: &WorkflowFile,
    output: &Value,
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
        let Some(destination) = &node.destination else {
            continue;
        };
        // An output node the run never reached (untaken branch, or a path that
        // paused for approval) is not a delivery that failed — it is a delivery
        // that was never owed. No attempt, no row.
        if !node_was_reached(output, &node.id) {
            tracing::debug!(
                company = %record.id,
                workflow = %workflow.id,
                node = %node.id,
                "workflow delivery: output node not reached; nothing to deliver"
            );
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
    }

    reports
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
            let admins = admin_emails(delivery.users.as_ref(), &record.id).await;
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
                // always-present operator channel so the owner still hears about
                // it. Never a silent no-op.
                _ => {
                    let (why, why_reason) = if delivery.mail.is_none() {
                        (
                            "no mailbox is configured for this company",
                            DeliveryReason::OwnerFellBackNoMailbox,
                        )
                    } else {
                        (
                            "no active admin has an email address",
                            DeliveryReason::OwnerFellBackNoAdminAddress,
                        )
                    };
                    reports.push(
                        post_to_channel(
                            delivery,
                            crate::runtime::channel::OPERATOR_CHANNEL,
                            subject,
                            text,
                        )
                        .await
                        .map(|()| {
                            row(
                                Some(crate::runtime::channel::OPERATOR_CHANNEL.to_string()),
                                DeliveryStatus::Sent,
                                why_reason,
                                format!("{why}, so the report went to the operator channel"),
                            )
                        })
                        // The channel's own failure class (`_class`) is dropped
                        // in favour of naming the fallback, which is the part an
                        // operator reading a host log needs: the interesting
                        // fact is that `owner` had nowhere left to go. The full
                        // text, class included, is on `detail`.
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

    match park_effect(parking, &record.id, effect).await {
        Ok(()) => {
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

/// Parks `effect` on the gate and journals it — **both halves or neither**.
///
/// The gate is in-memory; the journal is the durable record `/approvals` reads
/// and boot replay rehydrates. A gate entry the journal never recorded is the
/// worst of the three possible outcomes: it shows up in the operator's queue
/// now, vanishes on the next restart, and backs a `pending` row that promises a
/// card which no longer exists.
///
/// Bundling the two handles in [`DeliveryParking`] makes the *mis-wiring* of
/// that state unrepresentable, but it does nothing about a **partial failure at
/// runtime** — `park` succeeding and `record_parked` erroring (a full disk, a
/// read-only volume, a serialization fault). So the journal write is treated as
/// the commit point: if it fails, the gate entry is retracted before returning
/// the error, and the caller degrades to the `skipped` row it already emits for
/// a parking failure.
///
/// Retraction has to undo **two** things, because a failed `record_parked` has
/// already mutated the journal's in-memory queue (it inserts before it appends,
/// so the entry is live even though nothing reached disk):
///
/// 1. [`ApprovalGate::resolve`] with [`Verdict::Deny`] — the trait's only
///    removal verb, and the honest one: this effect must never execute. It is
///    attributed to [`ActorKind::System`](crate::ports::types::ActorKind::System)
///    (the runtime itself, as boot replay and the TTL sweep are) rather than to
///    an operator who made no such decision.
/// 2. [`RuntimeJournal::record_resolved`] — which also removes before it
///    appends, so it clears the in-memory queue entry that would otherwise show
///    the operator a card `/approvals` lists but the gate can no longer
///    execute. Its own append will usually fail for the same reason the first
///    one did; that is fine and expected, since there is no `ApprovalParked`
///    line on disk for it to pair with anyway.
///
/// The ordering cannot simply be inverted to dodge this: `record_parked` needs
/// the [`ApprovalId`](crate::ports::types::ApprovalId) that `park` mints, so the
/// gate write must come first.
///
/// Both rollback steps deliberately ignore their own errors and the **original**
/// journal error propagates — the report is reported unsent either way, and
/// losing the real cause behind a cleanup error would make the failure harder to
/// diagnose, not easier.
async fn park_effect(
    parking: &DeliveryParking,
    company: &CompanyId,
    effect: Effect,
) -> Result<(), crate::error::OpenCompanyError> {
    let approval_id = parking.approvals.park(company, effect.clone()).await?;
    if let Err(err) = parking
        .journal
        .record_parked(&approval_id, &effect, now_millis())
        .await
    {
        // Roll back to "never parked". Both steps deliberately swallow their own
        // errors — `err` below is the one worth surfacing.
        if let Err(rollback) = parking
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
                "workflow delivery: a parked effect could not be journaled AND could not be \
                 retracted from the approval gate; it may linger in the queue until restart"
            );
        }
        // Clears the in-memory queue entry `record_parked` inserted before it
        // failed to write. Its append will usually fail too — expected, and
        // ignored: there is no `ApprovalParked` line on disk to pair with.
        let _ = parking.journal.record_resolved(&approval_id).await;
        return Err(err);
    }
    Ok(())
}

/// The active admins' email addresses, in store order. An unreadable user store
/// yields none (which routes `owner` to the operator-channel fallback) rather
/// than failing the run.
async fn admin_emails(users: &dyn UserStore, company: &CompanyId) -> Vec<String> {
    match users.list_users(company).await {
        Ok(list) => list
            .into_iter()
            .filter(|u| u.role == UserRole::Admin && u.status == UserStatus::Active)
            .map(|u| u.email)
            .filter(|email| email.contains('@'))
            .collect(),
        Err(err) => {
            tracing::warn!(
                company = %company,
                error = %err,
                "workflow delivery: could not read the user directory; falling back to the operator channel"
            );
            Vec::new()
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
        let wired: Vec<&str> = delivery
            .channels
            .iter()
            .map(|c| c.channel_id())
            .collect::<Vec<_>>();
        return Err((
            DeliveryReason::ChannelNotWired,
            if wired.is_empty() {
                format!("`{channel_id}` is not wired on this runtime, which has no channels at all")
            } else {
                format!(
                    "`{channel_id}` is not a wired channel — this runtime has: {}",
                    wired.join(", ")
                )
            },
        ));
    };
    adapter
        .send(OutboundMessage {
            task_id: None,
            channel: channel_id.to_string(),
            text: format!("{subject}\n\n{text}"),
            steps: Vec::new(),
            reply_to: None,
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
    use crate::runtime::channel::{OPERATOR_CHANNEL, OperatorChannel};
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
            template_provenance: None,
        }
    }

    fn smtp_creds() -> SmtpCredentials {
        SmtpCredentials {
            host: "smtp.example.test".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "acme".into(),
            password: "hunter2".into(),
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
        inbox: Arc<FsInboxStore>,
        users: Arc<FsOps>,
        company: CompanyId,
        /// The approvals queue, when [`with_parking`](Harness::with_parking)
        /// wired one. The real gate and a real on-disk journal, not fakes: the
        /// point of these tests is that a workflow's park lands in the same
        /// queue an agent's does.
        gate: Option<Arc<ManifestApprovalGate>>,
        journal: Option<Arc<RuntimeJournal>>,
    }

    impl Harness {
        fn new(dir: &std::path::Path, with_mail: bool, with_channel: bool) -> Self {
            let mail = RecordingMailSender::new();
            let inbox = Arc::new(FsInboxStore::new(dir));
            let users = Arc::new(FsOps::new(dir));
            let channel = OperatorChannel::new();
            let channels: Vec<Arc<dyn ChannelAdapter>> = if with_channel {
                vec![Arc::new(channel.clone())]
            } else {
                Vec::new()
            };
            Self {
                deps: WorkflowDeliveryDeps {
                    mail: with_mail.then(|| CompanyMail {
                        sender: Arc::new(mail.clone()),
                        smtp: smtp_creds(),
                    }),
                    inbox: inbox.clone(),
                    users: users.clone(),
                    channels,
                    parking: None,
                },
                mail,
                channel,
                inbox,
                users,
                company: CompanyId::new("acme"),
                gate: None,
                journal: None,
            }
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
            &reached_output(),
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
            &reached_output(),
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(h.mail.sent().len(), 1);
        assert_eq!(h.mail.sent()[0].1.to, "ada@acme.test");
    }

    /// With no mailbox wired, `owner` falls back to the always-present operator
    /// channel rather than becoming a silent no-op.
    #[tokio::test]
    async fn owner_falls_back_to_the_operator_channel_without_mail() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, true);
        h.add_admin("u1", "ada@acme.test").await;

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            &reached_output(),
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent);
        assert_eq!(reports[0].target.as_deref(), Some(OPERATOR_CHANNEL));
        assert!(reports[0].detail.contains("no mailbox"), "{reports:?}");
        let sent = h.channel.sent();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Q3 is up 12%."), "{}", sent[0].text);
    }

    /// A company with a mailbox but no admin address also falls back — the owner
    /// still hears about it.
    #[tokio::test]
    async fn owner_falls_back_when_no_admin_has_an_address() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), true, true);

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            &reached_output(),
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent);
        assert!(reports[0].detail.contains("no active admin"), "{reports:?}");
        assert!(h.mail.sent().is_empty(), "nothing should have been emailed");
        assert_eq!(h.channel.sent().len(), 1);
    }

    /// Both fallbacks unavailable: no mail, no operator channel. Still a row —
    /// `failed`, naming the gap — never silence.
    #[tokio::test]
    async fn owner_with_neither_mail_nor_a_channel_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path(), false, false);

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("owner", None),
            &reached_output(),
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Failed);
        assert!(reports[0].detail.contains("operator"), "{reports:?}");
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
    /// `[tools]` section at all now defaults to `["*", "media", "composio"]`,
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
            &reached_output(),
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
        let h = Harness::new(dir.path(), true, true);

        let reports = deliver_outputs(
            Some(&h.deps),
            &record(&[]),
            &graph("channel", Some(OPERATOR_CHANNEL)),
            &reached_output(),
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Sent);
        let sent = h.channel.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel, OPERATOR_CHANNEL);
        assert!(sent[0].text.contains("Q3 is up 12%."));
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
            &reached_output(),
        )
        .await;

        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].status, DeliveryStatus::Failed);
        assert!(
            reports[0].detail.contains("not a wired channel"),
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
            &output,
        )
        .await;

        assert!(reports.is_empty(), "{reports:?}");
        assert!(h.mail.sent().is_empty());
        assert!(h.channel.sent().is_empty());
    }

    /// An `output` node with no `destination` is the pre-#170 shape: it still
    /// shows in the run drawer and produces no delivery row.
    #[tokio::test]
    async fn an_output_node_without_a_destination_produces_no_row() {
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

        let reports =
            deliver_outputs(Some(&h.deps), &record(&["*"]), &plain, &reached_output()).await;
        assert!(reports.is_empty(), "{reports:?}");
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
            &reached_output(),
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
}
