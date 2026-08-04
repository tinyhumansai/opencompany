//! The [`WorkflowRunner`] port: execute a company's workflow graph.
//!
//! A company's workflows are data-only
//! [`WorkflowFile`](crate::company::workflow_file::WorkflowFile) graphs. Running
//! one is dependency-inverted behind this port so the kernel and the HTTP layer
//! depend only on the trait: the concrete engine-backed implementation
//! (`crate::workflows::HarnessWorkflowRunner`, which drives the graph on the
//! embedded `tinyflows` engine with agent nodes on the harness pool) is compiled
//! only under `feature = "openhuman"`. The default build compiles this trait and
//! its result type but wires no implementation — a runtime with no runner leaves
//! the run route reporting "not wired", exactly like the other networked seams.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::company::WorkflowFile;
use crate::ports::types::CompanyId;

/// The outcome of running one workflow to completion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    /// The final run state after the terminal node(s) completed. Its shape is
    /// the engine's `{ "run": …, "nodes": { "<id>": { "items": [ … ] } } }` map.
    pub output: Value,
    /// Node ids that paused the run awaiting human approval. Empty for a run
    /// that reached its terminal node(s) without gating.
    pub pending_approvals: Vec<String>,
    /// One row per attempt to route a reached `output` node's report to its
    /// configured destination (issue #170), in graph order.
    ///
    /// Empty for a graph whose `output` nodes name no destination — the
    /// pre-#170 shape — which is why it is `#[serde(default)]`: a `WorkflowRun`
    /// deserialized from an older payload still loads.
    ///
    /// A delivery failure is reported here rather than failing the run: the work
    /// the run did is still valid. An output node the run never reached
    /// contributes no row at all, so an absent row means "not reached", never
    /// "silently dropped".
    #[serde(default)]
    pub deliveries: Vec<DeliveryReport>,
}

/// What became of one attempt to deliver an `output` node's report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatus {
    /// The transport accepted the report.
    Sent,
    /// Parked for operator approval and not sent — the destination needs a
    /// human verdict before anything leaves the process (a cold email
    /// recipient, which a workflow may not cold-open by itself).
    ///
    /// **This row is a snapshot taken at run time, not a live status.** A
    /// workflow run is not persisted, so nothing ever comes back to flip this
    /// row to `Sent` once the operator approves. The approvals queue is the
    /// live source of truth: the parked effect is journal-backed, survives a
    /// restart, and executes on approval through the same path an agent's
    /// `email.send` does. Read this row as "an approval was opened for this",
    /// then look at Approvals for what became of it.
    Pending,
    /// Deliberately not attempted — a policy precondition was unmet (no mailbox
    /// configured, or a cold recipient on a runtime that cannot park). Not an
    /// error; the report simply was not owed to that address under the current
    /// rules.
    Skipped,
    /// Refused by policy: the company does not grant what the destination needs.
    Denied,
    /// Attempted (or attemptable) and did not work — a transport error, an
    /// unwired channel, or a runtime with no delivery ports at all.
    Failed,
}

/// Why a delivery attempt came out the way it did, as a closed set (issue
/// #248).
///
/// # Why this is an enum and not another string
///
/// [`DeliveryReport::detail`] is free text, and on the transport-failure arms it
/// interpolates the transport's own words. A mail transport's refusal routinely
/// quotes the mailbox it refused (an SMTP `550`/`553` reply commonly reads
/// `<recipient@example.invalid>: Recipient address rejected`), so `detail`
/// carries a recipient address on exactly the paths an operator most wants to
/// read about. That is fine on the operator's own surfaces — the run response
/// and the `WorkflowRunFinished` history a tenant reads back — and not fine on
/// host stdout, which on a hosted deployment is the platform rather than the
/// operator.
///
/// This enum is the half that may be logged. It is *unable* to carry
/// transport-supplied text: it has no `String` payload, so there is no
/// `format!` that produces one. The guarantee is the compiler's rather than a
/// reviewer's, and a new delivery outcome cannot be added without classifying
/// it — the construction sites match on this type exhaustively.
///
/// Its [`Display`](std::fmt::Display) rendering is the prose that reaches the
/// log; its serde name is the stable token for querying a run history.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryReason {
    /// This build wired no delivery ports at all, so nothing could be sent.
    NotWired,
    /// An `owner` report reached the company's admin mailbox.
    OwnerEmailed,
    /// An `email` report reached the named recipient on an established thread.
    RecipientEmailed,
    /// The mail transport refused the message. **The transport's own reason is
    /// in `detail`, not here** — that is the string that quotes the address.
    MailTransportRefused,
    /// `owner` had no mailbox to send from, so the report went to the operator
    /// channel instead.
    OwnerFellBackNoMailbox,
    /// `owner` had a mailbox but no active admin with an address, so the report
    /// went to the operator channel instead.
    OwnerFellBackNoAdminAddress,
    /// `owner`'s operator-channel fallback itself failed, so nothing was sent.
    OwnerFallbackFailed,
    /// The company's `[tools].allow` does not grant `email`, so a workflow may
    /// not mail a named address.
    EmailNotGranted,
    /// No mailbox is configured for the company, so there was nothing to send
    /// from.
    NoMailboxConfigured,
    /// The recipient is not an established thread and this runtime has no
    /// approvals queue to park the send on.
    RecipientNotEstablished,
    /// The recipient is not an established thread; the send is parked in
    /// Approvals awaiting a human verdict.
    ParkedForApproval,
    /// The recipient is not an established thread and the send could not be
    /// parked for approval either.
    ParkingUnavailable,
    /// A `channel` report was posted to the wired adapter.
    ChannelPosted,
    /// The destination names a channel this deployment never wired. **Which
    /// channel, and what is wired instead, is in `detail`** — a channel id is
    /// the `channel` arm's target, and targets do not go to host logs.
    ChannelNotWired,
    /// The channel adapter refused the message. As with mail, the adapter's own
    /// reason stays in `detail`.
    ChannelRefused,
    /// The destination kind is not one this runtime knows how to deliver to
    /// (unreachable through `parse_workflow`, which rejects unknown kinds).
    UnknownDestinationKind,
    /// No reason was recorded. Only reachable by deserializing a
    /// `WorkflowRunFinished` event written before this field existed.
    #[default]
    Unspecified,
}

impl std::fmt::Display for DeliveryReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Every arm is a literal. Keep it that way: the whole point of the type
        // is that nothing runtime-supplied can reach a host log through it.
        f.write_str(match self {
            Self::NotWired => "report delivery is not wired on this runtime",
            Self::OwnerEmailed => "emailed the company's admin",
            Self::RecipientEmailed => "emailed the named recipient on an established thread",
            Self::MailTransportRefused => "the mail transport refused the message",
            Self::OwnerFellBackNoMailbox => {
                "no mailbox is configured for this company, so the report went to the operator \
                 channel"
            }
            Self::OwnerFellBackNoAdminAddress => {
                "no active admin has an email address, so the report went to the operator channel"
            }
            Self::OwnerFallbackFailed => "the operator channel fallback failed",
            Self::EmailNotGranted => {
                "this company's [tools].allow does not grant `email`, so a workflow may not send \
                 mail to a named address"
            }
            Self::NoMailboxConfigured => "no mailbox is configured for this company",
            Self::RecipientNotEstablished => {
                "the recipient has never written to the company, and this runtime has no approvals \
                 queue to park the send on"
            }
            Self::ParkedForApproval => {
                "the recipient has never written to the company, so the report is waiting in \
                 Approvals"
            }
            Self::ParkingUnavailable => {
                "the recipient has never written to the company, and the report could not be \
                 queued for approval either"
            }
            Self::ChannelPosted => "posted to the channel",
            Self::ChannelNotWired => "the destination channel is not wired on this runtime",
            Self::ChannelRefused => "the channel refused the message",
            Self::UnknownDestinationKind => {
                "the destination kind is not one this runtime can deliver to"
            }
            Self::Unspecified => "no reason was recorded for this delivery",
        })
    }
}

/// One attempt to route a reached `output` node's report somewhere.
///
/// On an on-demand run these rows ride the run response into the console's
/// run-result panel, so an operator can tell a delivered report from an
/// undelivered one without reading a log. A scheduled run's rows are journaled
/// as a `WorkflowRunFinished` event (issue #228) that the tenant's own console
/// reads back, and are summarized on host stdout by the scheduler.
///
/// **The two reason fields are not interchangeable.** See [`DeliveryReason`]:
/// `detail` is for the operator's surfaces and may quote a transport; `reason`
/// is the only one that may reach a host log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryReport {
    /// The `output` node whose report this was.
    pub node: String,
    /// The destination kind as authored (`owner` / `email` / `channel`).
    pub kind: String,
    /// The address or channel actually addressed. For `owner` this is the
    /// server-resolved recipient, not something the graph named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// What became of the attempt.
    pub status: DeliveryStatus,
    /// An operator-readable reason, always populated — including on success, so
    /// a `sent` row still says *how* it was sent (which matters for `owner`,
    /// whose recipient the graph never named).
    ///
    /// **Operator surfaces only — never a host log.** On the transport-failure
    /// arms this interpolates the transport's own text, which routinely quotes
    /// the address it refused. Log [`Self::reason`] instead.
    ///
    /// "Operator surfaces" is a claim about specific readers, so it is worth
    /// naming them. This field reaches: the run response, the company-scoped
    /// SSE projection, and `GET …/workflows/runs` — all three behind the
    /// `ScopedCompany` guard, and all three reading a **per-company**
    /// `events.jsonl`. It reaches neither of the journal's two non-tenant
    /// readers: the inference-sidecar wire-out (`brain::medulla::effects`) and
    /// the orchestrator's insight summary (`harness::orchestrator`, compiled
    /// only under `openhuman`) both fold a finished run to counts, and tests
    /// pin that they do.
    pub detail: String,
    /// The same outcome as a closed set, safe to log by construction.
    ///
    /// `#[serde(default)]` so a `WorkflowRunFinished` event journaled before
    /// this field existed still loads, as [`DeliveryReason::Unspecified`].
    #[serde(default)]
    pub reason: DeliveryReason,
}

/// Runs a company's workflow graph to completion.
///
/// `company` names the tenant whose roster the run's agent nodes execute on;
/// `workflow` is the parsed graph; `input` is the trigger payload (an arbitrary
/// JSON value seeded as the trigger node's item).
#[async_trait]
pub trait WorkflowRunner: Send + Sync {
    /// Runs `workflow` for `company` with the trigger `input`, returning the
    /// final state and any nodes left pending approval.
    async fn run(
        &self,
        company: &CompanyId,
        workflow: &WorkflowFile,
        input: Value,
    ) -> Result<WorkflowRun>;
}
