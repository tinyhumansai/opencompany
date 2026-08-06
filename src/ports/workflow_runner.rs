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

use std::sync::Arc;

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
    /// Whether an operator stopped this run before it reached its terminal
    /// node(s) (issue #383).
    ///
    /// **A cancelled run is not a failed one.** The graph did nothing wrong and
    /// the host did not go away — a human decided it had seen enough. That
    /// distinction is the point of a separate flag rather than an `error`
    /// string: the console's three terminal wordings ("failed", "interrupted by
    /// a host restart", "stopped by an operator") stay distinguishable, and a
    /// deliberate stop never lands in the failure count.
    ///
    /// `#[serde(default)]` so a `WorkflowRun` deserialized from a pre-#383
    /// payload still loads, as `false`.
    #[serde(default)]
    pub cancelled: bool,
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
    /// configured, or a cold recipient on a runtime that cannot park), or a run
    /// earlier in this lineage already delivered this node's report and the
    /// continuation must not send it twice (issue #438). Not an error; the
    /// report simply was not owed to that address under the current rules.
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
    /// A run earlier in this lineage already delivered this node's report, so
    /// the continuation did not send it again (issue #438).
    ///
    /// Resuming an approved gate is a **re-run** — the engine settles when it
    /// pauses, so continuing means walking the graph again from the trigger.
    /// Every `output` node upstream of the gate is therefore reached a second
    /// time, and without this the operator's approval would mail the same
    /// report to the same person twice. The ledger of what a lineage has
    /// already sent rides the continuation's trigger input; see
    /// [`crate::runtime::workflow_resume`].
    ///
    /// A **parked** report counts as delivered too: the card is durable and
    /// approving it sends, so re-parking would stack a second identical card
    /// and approving both would send twice.
    AlreadyDelivered,
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
            Self::AlreadyDelivered => {
                "an earlier run in this workflow's approval lineage already delivered this report, \
                 so it was not sent again"
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

/// What the *caller* knows about a run that the runner cannot work out for
/// itself (issue #371).
///
/// # Why the entry point mints the id rather than the runner
///
/// The run id correlates a run's progress events with its outcome, and the
/// outcome is journaled by the **caller** —
/// [`record_run_finished`](crate::runtime::record_run_finished) — on *both*
/// arms. On the error arm the runner returns nothing at all, so a runner-minted
/// id would be lost exactly when it is most needed: a failed run's node events
/// would be orphaned from the `WorkflowRunFinished` that carries the reason it
/// failed, and the console could not say how far the run got before it died.
/// Minting here, above the call, is what makes the two halves share one id on
/// every path.
///
/// This is a **crate-internal port type**, not a wire type: it has no serde
/// impl and no HTTP surface. Adding it to
/// [`WorkflowRunner::run`](WorkflowRunner::run) makes the compiler enumerate
/// every entry point, which is the whole point — an entry point that forgot to
/// mint an id would silently journal an uncorrelatable run.
#[derive(Clone, Debug)]
pub struct WorkflowRunContext {
    /// The correlation id for this run, minted by the entry point.
    pub run_id: String,
    /// Whether a cron schedule started this run rather than an operator. Rides
    /// the run's [`WorkflowRunStarted`](crate::ports::types::CompanyEvent)
    /// event so the console can tell a nobody-was-watching run from a Run-button
    /// one *while it is happening*, not only once it settles.
    pub scheduled: bool,
    /// The stop signal for this run (issue #383).
    ///
    /// A context built by
    /// [`RunSupervisor::begin`](crate::runtime::RunSupervisor::begin) shares this
    /// handle with the supervisor's registry, so
    /// `POST …/workflows/runs/{runId}/cancel` can reach a run that is still
    /// walking its graph. A context built by [`new`](Self::new) carries a handle
    /// nobody else holds — nothing can ever fire it, which is exactly right for
    /// a test or an entry point that opted out of registration.
    pub cancel: RunCancel,
}

/// A one-way stop signal for one workflow run (issue #383).
///
/// # Why this is not the engine's `CancellationToken`
///
/// tinyflows *has* a `CancellationToken`, but no public entry point accepts one
/// **together with** a `RunObserver`: `run_cancellable` hardcodes a
/// `NoopObserver`, `run_with_observer` hardcodes a fresh token, and
/// `build_and_run` — which takes both — is private. Reaching for the engine's
/// token would therefore cost every cancellable run its per-node progress
/// events (issue #371), which is a strictly worse trade than stopping the run
/// host-side. So the runner selects on this signal instead and drops the engine
/// future when it fires.
///
/// It also could not live here even if the engine's would do: this type is on
/// the port, and the port compiles in the **default build**, which links no
/// `tinyflows` at all.
///
/// # Semantics
///
/// Firing this stops the run **mid-await**, it does not let the current node
/// finish. A node that was half way through an external side effect stays half
/// way through it — the same class of outcome as the host being killed, which
/// the boot sweep already handles, only operator-initiated and therefore more
/// frequent. "Stopped", not "finished".
///
/// Cheap to [`Clone`] (a shared handle); firing is idempotent and safe from any
/// thread.
#[derive(Clone)]
pub struct RunCancel {
    /// A `watch` rather than a `Notify`: the runner has to be able to *await*
    /// the signal inside a `select!`, and it must not matter whether the cancel
    /// arrives before or after that await is first polled. A watch channel
    /// carries the state, so a cancel that lands first is still observed;
    /// `Notify` only carries an edge. The [`Arc`] keeps the sender alive for as
    /// long as any handle exists, so a receiver can never see the channel close.
    tx: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Default for RunCancel {
    fn default() -> Self {
        Self {
            tx: Arc::new(tokio::sync::watch::channel(false).0),
        }
    }
}

impl std::fmt::Debug for RunCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunCancel")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl RunCancel {
    /// An un-fired signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fires the signal. Idempotent, and infallible by construction —
    /// `send_replace` does not care whether anyone is listening, so cancelling a
    /// run whose future has already settled is a silent no-op rather than an
    /// error the caller has to decide what to do about.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    /// Whether the signal has fired.
    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once the signal has fired — **including when it fired before
    /// this was first polled**, which is the whole reason for the watch channel.
    ///
    /// Cancel-safe: it holds no state of its own between polls, so dropping it
    /// (which is what `select!` does to the losing branch) loses nothing.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            if rx.changed().await.is_err() {
                // Unreachable: `self` holds an `Arc` of the sender, so the
                // channel cannot close while this future exists. Park rather
                // than return, because returning would read as "cancelled" to
                // every caller and stop a healthy run.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Equality over the run's **identity**, not its live state.
///
/// [`RunCancel`] is a shared handle whose value changes under both sides, so
/// including it would make two clones of one context compare unequal the moment
/// one of them was cancelled. The id and the scheduled flag are what callers
/// (and tests) actually mean by "the same run context".
impl PartialEq for WorkflowRunContext {
    fn eq(&self, other: &Self) -> bool {
        self.run_id == other.run_id && self.scheduled == other.scheduled
    }
}

impl Eq for WorkflowRunContext {}

impl WorkflowRunContext {
    /// A context with a freshly minted run id and an unregistered stop signal.
    ///
    /// `scheduled` says whether a cron started the run rather than an operator.
    ///
    /// **The run this builds cannot be cancelled from the console**, because
    /// nothing but this context holds the signal. Entry points that want an
    /// operator to be able to stop the run go through
    /// [`RunSupervisor::begin`](crate::runtime::RunSupervisor::begin) instead,
    /// which mints the same thing and registers the handle. This constructor
    /// stays for tests and for any caller that deliberately runs unregistered.
    pub fn new(scheduled: bool) -> Self {
        Self {
            run_id: crate::ports::ids::generate_id(),
            scheduled,
            cancel: RunCancel::new(),
        }
    }
}

/// Runs a company's workflow graph to completion.
///
/// `company` names the tenant whose roster the run's agent nodes execute on;
/// `workflow` is the parsed graph; `input` is the trigger payload (an arbitrary
/// JSON value seeded as the trigger node's item); `ctx` carries the caller's run
/// id and whether a cron started the run.
#[async_trait]
pub trait WorkflowRunner: Send + Sync {
    /// Runs `workflow` for `company` with the trigger `input`, returning the
    /// final state and any nodes left pending approval.
    async fn run(
        &self,
        company: &CompanyId,
        workflow: &WorkflowFile,
        input: Value,
        ctx: &WorkflowRunContext,
    ) -> Result<WorkflowRun>;
}
