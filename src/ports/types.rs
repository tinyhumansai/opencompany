//! Shared id, enum, and payload types referenced across more than one port.
//!
//! Types local to a single port live beside that port's trait; everything the
//! kernel threads between ports (ids, events, effects, cycle payloads) lives
//! here. Every type derives `Clone, Debug, Serialize, Deserialize` so it can
//! round-trip through JSONL persistence and the HTTP surface.
//!
//! Field lists are Phase-1-minimal: the port contract in
//! `docs/spec/runtime/ports.md` binds trait and method names, and permits
//! payload fields to evolve within Phase 1.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::company::{CompanyManifest, POLICY_MODES, Policy};
use crate::ports::ids::{agent_slug, generate_id, now_millis};
use crate::ports::workflow_runner::{
    DeliveryReport, WorkflowBlockedNode, WorkflowRunApprovalRow, WorkflowRunBoardRow,
};

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable identifier for a company (typically a slug of its name).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompanyId(String);

impl CompanyId {
    /// Wraps an existing id string (e.g. a manifest-derived slug).
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Mints a fresh process-unique company id.
    pub fn generate() -> Self {
        Self(generate_id())
    }
}

impl From<String> for CompanyId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for CompanyId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CompanyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonic sequence number for an event within a company's log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventSeq(u64);

impl EventSeq {
    /// Wraps a raw sequence value.
    pub fn new(seq: u64) -> Self {
        Self(seq)
    }

    /// The underlying sequence value.
    pub fn value(self) -> u64 {
        self.0
    }
}

impl From<u64> for EventSeq {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for EventSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifier for a parked effect awaiting operator approval.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApprovalId(String);

impl ApprovalId {
    /// Wraps an existing approval id string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Mints a fresh approval id (called by the gate at park time).
    pub fn generate() -> Self {
        Self(generate_id())
    }
}

impl From<String> for ApprovalId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for ApprovalId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ApprovalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content address of a stored context chunk.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkAddr(String);

impl ChunkAddr {
    /// Wraps a content-address string (minted by the context store).
    pub fn new(addr: impl Into<String>) -> Self {
        Self(addr.into())
    }
}

impl From<String> for ChunkAddr {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for ChunkAddr {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ChunkAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque per-company secret value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretValue(pub String);

impl SecretValue {
    /// Borrows the underlying secret string.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Actors and verdicts
// ---------------------------------------------------------------------------

/// Who performed an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    /// The human operator.
    Operator,
    /// The runtime itself (timers, boot replay).
    System,
    /// An autonomous agent inside the company.
    Agent,
    /// A human collaborator of the company. The user's id lives in
    /// [`Actor::id`].
    ///
    /// Fieldless on purpose: `ActorKind` is `Copy`, and a variant carrying a
    /// `String` would silently take that away from every existing holder.
    User,
}

/// An identified actor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    /// The category of actor.
    pub kind: ActorKind,
    /// A stable id for the actor within its category.
    pub id: String,
}

/// An operator's resolution of a parked approval.
///
/// The HTTP body uses the lowercase strings `"approve"` / `"deny"`. The
/// `edit` path named in `approvals.md` is intentionally absent — the api.md
/// body defines no such verdict in Phase 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Approve and execute the parked effect.
    Approve,
    /// Deny and discard the parked effect.
    Deny,
}

/// What the operator says one chat message is **for** (issue #1152).
///
/// # Why this is not a third `TaskDeliverable`
///
/// [`TaskDeliverable`](crate::ports::tasks::TaskDeliverable) answers a question
/// about a **card**: once the work exists, does doing it produce a one-off
/// result or a reusable workflow. This answers the question one step earlier —
/// whether the message is a request for work at all. A card can never *be* "not
/// work", so a third `TaskDeliverable` variant would be
/// representable-but-invalid in every position that field is stored and read
/// (`TaskRecord::deliverable`, the builder pass's dispatch check, the console's
/// task edit dialog), and every one of those readers would owe a branch for a
/// state that cannot occur.
///
/// [`deliverable`](Self::deliverable) returning `None` for [`Chat`](Self::Chat)
/// is that same statement, made in the type: no card, therefore no deliverable
/// to choose.
///
/// # The operator chooses; nothing guesses
///
/// Like `TaskDeliverable` (decision D2a of issue #580), this is only ever set
/// from an explicit control a person pressed. Nothing reads a message and
/// decides it "looks like chatter" — that judgement already exists one layer
/// down, as the lexical triage (issue #267) and the model escalation (issue
/// #984), and this is deliberately not a third one of those. It is the person's
/// own statement about their own message, settled before any model runs.
///
/// Additive on the wire: the two work values are the exact `TaskDeliverable`
/// words, so every `deliverable` value ever journaled on a
/// [`CompanyEvent::OperatorMessage`] still deserializes, and a message that
/// carried no choice still serializes with the field absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageIntent {
    /// Not a request for work — the composer's "Just chatting". No
    /// deterministic path opens a card for this message.
    Chat,
    /// A request for work, done once. The historical default: identical in
    /// behaviour, and on the wire, to a message that expressed no choice.
    Once,
    /// A request for work, built as a reusable workflow. The card it opens
    /// routes through the builder pass.
    Workflow,
}

impl MessageIntent {
    /// The deliverable a card opened for this message carries, or `None` when
    /// the operator said the message asks for no card at all.
    pub fn deliverable(self) -> Option<crate::ports::tasks::TaskDeliverable> {
        match self {
            Self::Chat => None,
            Self::Once => Some(crate::ports::tasks::TaskDeliverable::Once),
            Self::Workflow => Some(crate::ports::tasks::TaskDeliverable::Workflow),
        }
    }

    /// Whether the operator stated this message is not a request for work.
    pub fn is_chat(self) -> bool {
        matches!(self, Self::Chat)
    }

    /// The wire word, for a log line or a note a person reads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Once => "once",
            Self::Workflow => "workflow",
        }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// An external stimulus fed into a company's cycle loop.
///
/// Serialized internally-tagged under `kind` so each JSONL line is
/// self-describing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CompanyEvent {
    /// A human sent a chat message.
    OperatorMessage {
        /// The message text.
        text: String,
        /// Who sent it.
        ///
        /// `None` on events journaled before per-user auth existed, and on
        /// sends made with an operator/platform credential, which have no
        /// person behind them. Both are attributed to "operator" on read.
        ///
        /// `#[serde(default)]` is what lets every already-persisted event load;
        /// `skip_serializing_if` is what keeps an unattributed event
        /// serializing byte-for-byte as it did before this field existed, so
        /// export/import and the cross-backend round-trip need no migration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
        /// The desk / chat thread the message targets (issue #53), so the
        /// orchestrator brain can route an addressed message to that desk's lead
        /// member and journal replies against it. `None` on an unaddressed
        /// message (routed to the orchestrator) and on every event journaled
        /// before this field existed. Like `by`, `skip_serializing_if` keeps a
        /// pre-existing event byte-identical, so no stored record needs
        /// migrating.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat: Option<String>,
        /// The message this one replies to, as that message's own sequence
        /// position (issue #364) — what makes a thread reply survive a reload.
        ///
        /// A **parent id, not a thread object**: the console already folds a
        /// transcript by parent, so a thread is exactly "the messages pointing
        /// at this one". Recording it that way costs one optional field and
        /// needs no lifecycle, no membership, and no second addressing scheme
        /// beside `chat`, which stays the channel the whole thread lives in.
        ///
        /// `None` on a message posted straight into a channel — which is every
        /// message journaled before this field existed, and correctly so: they
        /// were never thread replies. Additive on exactly the `by` / `chat`
        /// terms above, so no stored record migrates.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<EventSeq>,
        /// What the operator's composer said this message is **for** (issues
        /// #845, #1152), as they set it — [`MessageIntent::Workflow`] when they
        /// picked "Build me the workflow", [`MessageIntent::Chat`] when they
        /// picked "Just chatting".
        ///
        /// Carried on the event because the cycle is the only place that holds
        /// both this fact and the turn about to answer, and without it the turn
        /// answered blind. A `workflow` message opens a card that the **builder
        /// pass** owns ([`crate::harness::workflow_build`]) — the chat cycle
        /// still runs, in parallel, and the desk agent answering it has no
        /// authoring tool and correctly says so. So the operator was told "I
        /// can't build the workflow" *while a proposal for it was being built*.
        /// [`CompanyRuntime::inject_workflow_builder_awareness`](crate::company::runtime::CompanyRuntime)
        /// reads this to tell the turn who owns the authoring.
        ///
        /// Typed [`MessageIntent`] rather than
        /// [`TaskDeliverable`](crate::ports::tasks::TaskDeliverable) since
        /// #1152, because the operator can now say the message is not a request
        /// for work at all — which is a statement about the *message*, not a
        /// choice of what a card produces. The field keeps its name and its wire
        /// key: one operator choice, one field, so `{"deliverable":"workflow"}`
        /// and "just chatting" cannot both be asserted about one message.
        ///
        /// `None` means the caller expressed no choice — every message journaled
        /// before this field existed, and every non-chat producer. Additive on
        /// exactly the `by` / `chat` / `parent` terms above, and the two work
        /// words are unchanged, so no stored record migrates.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deliverable: Option<MessageIntent>,
    },
    /// A turn was **accepted** for an operator message (issue #983) — the
    /// transcript line that says the company took the work on.
    ///
    /// # Why this is not the run row
    ///
    /// Stage 1 mints a [`RunRecord`](crate::ports::runs::RunRecord) for the same
    /// turn, and the two answer different questions. The row answers *status* —
    /// pending, running, failed, what it cost — and is read by a poll. This is
    /// the *transcript*: the log is the one place a reader reconstructs what
    /// happened in a conversation from, and "a turn was accepted for this
    /// message" cannot be inferred from the log without it. An
    /// [`OperatorMessage`](Self::OperatorMessage) with no reply after it is
    /// indistinguishable from a chatter message that legitimately produced none,
    /// so the absence of an answer is not evidence of a lost turn — until this
    /// event makes the acceptance explicit.
    ///
    /// **Structural only.** No message text: the text is already on the
    /// `OperatorMessage` this brackets, and putting it here would be a second
    /// copy to redact.
    ///
    /// Additive: an entirely new `kind`, so no journal written before it existed
    /// carries it, and its presence changes how no existing variant serializes.
    TurnStarted {
        /// The turn's id — the same id its
        /// [`RunRecord`](crate::ports::runs::RunRecord) is keyed on, so a
        /// transcript line and a status row join without a second scheme.
        turn_id: String,
        /// The desk / chat thread the message was addressed to.
        chat_id: String,
        /// The message being replied to, when the turn answers a thread reply —
        /// the same sequence position
        /// [`OperatorMessage::parent`](Self::OperatorMessage) carries.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<EventSeq>,
        /// Who asked, mirroring
        /// [`OperatorMessage`](Self::OperatorMessage)'s `by`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// A turn that was accepted did not produce an answer (issue #983).
    ///
    /// The closing bracket of [`TurnStarted`](Self::TurnStarted), written by the
    /// turn itself when its cycle errors and by the boot sweep
    /// ([`crate::runtime::sweep_interrupted_turns`]) for a turn the host died
    /// under. Without it a turn killed with the pod is permanent silence: the
    /// question is in the transcript, no answer ever follows it, and nothing
    /// says why.
    TurnFailed {
        /// The turn this settles — the id its
        /// [`TurnStarted`](Self::TurnStarted) carries.
        turn_id: String,
        /// Why, in plain language. Tenant-scoped like
        /// [`WorkflowRunFinished::error`](Self::WorkflowRunFinished), and
        /// deliberately **not** projected onto the operator SSE stream.
        error: String,
    },
    /// One task attempt changed status (issue #1015).
    ///
    /// **The whole status machine, from one seam.** Emitted by the store
    /// decorator that wraps `put_run` — the single write primitive every
    /// [`RunStatus`](crate::ports::runs::RunStatus) change passes through, since
    /// `begin_run` and `finish_run` are trait defaults that call it and no
    /// backend overrides either. So the frame cannot be missed by adding a
    /// caller, which is what makes this a complete surface rather than a partial
    /// one.
    ///
    /// That matters most for the path the obvious seam misses:
    /// [`reap_orphaned_runs`](crate::ports::runs::reap_orphaned_runs) settles
    /// crash-killed runs by calling `finish_run` **directly**, never through the
    /// cycle. Emitting from the cycle's call sites would leave exactly those
    /// runs — the ones whose visibility the reaper exists to provide —
    /// transitioning in silence.
    ///
    /// **Not [`TurnStarted`](Self::TurnStarted)/[`TurnFailed`](Self::TurnFailed)**,
    /// though `turn_id` and this `run_id` are the same key. Those two are
    /// appended only on the chat HTTP path and bracket an
    /// [`OperatorMessage`](Self::OperatorMessage); firing one for a task attempt
    /// would put a turn in the chat transcript that no one took.
    ///
    /// Additive: a new `kind`, absent from every journal written before it.
    RunStatusChanged {
        /// The attempt's id — the same id
        /// [`RunRecord::id`](crate::ports::runs::RunRecord::id) is keyed on.
        run_id: String,
        /// The card this attempt is at, when it is a task attempt rather than a
        /// chat turn. Absent for a chat turn, which belongs to no card.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        /// The attempt ordinal at that card — `1` for the first.
        attempt: u32,
        /// The status moved from. Absent when the row is being minted, which is
        /// the one write with no prior state rather than a transition.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// The status moved to.
        to: String,
        /// Why, on a failure. Tenant-scoped like
        /// [`WorkflowRunFinished::error`](Self::WorkflowRunFinished) and
        /// deliberately **not** projected onto the operator SSE stream.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// An inbound webhook fired.
    WebhookReceived {
        /// The channel the webhook arrived on.
        channel: String,
        /// The raw webhook body.
        body: serde_json::Value,
    },
    /// A cron schedule fired.
    ScheduleFired {
        /// The cron expression that fired.
        cron: String,
        /// The prompt delivered to the company.
        prompt: String,
    },
    /// An A2A task was received from another agent.
    A2aTaskReceived {
        /// The sending agent's address.
        from: String,
        /// The task payload.
        task: serde_json::Value,
    },
    /// An effect was parked for the operator's sign-off (issue #379).
    ///
    /// The counterpart to [`ApprovalResolved`](Self::ApprovalResolved), and the
    /// reason it exists: parking was journal-only, so a console learned about a
    /// new request only when its feed next polled — far too late to raise the
    /// request inside the conversation that produced it.
    ///
    /// **Deliberately thin.** An id, a dotted kind, and the thread it came from
    /// — no payload, no asker. The parked effect's arguments are redacted and
    /// bounded exactly once, in
    /// [`pending_approvals`](crate::company::CompanyRuntime::pending_approvals),
    /// and putting them on a durable event would open a second surface that has
    /// to redact. A reader reacts to this by re-reading the approvals feed,
    /// which is where the safe projection lives.
    ApprovalParked {
        /// The approval now awaiting the operator.
        approval_id: ApprovalId,
        /// The parked effect's dotted kind, e.g. `payment.send`. Enough for a
        /// reader to decide whether it cares before it re-reads the feed.
        ///
        /// Named `effect_kind` rather than `kind` because `CompanyEvent` is
        /// serialized internally-tagged **under `kind`** — a variant field of
        /// that name collides with the tag and does not compile. The projected
        /// SSE frame calls it `kind` regardless: that envelope discriminates on
        /// `type`, so the short name is free there.
        effect_kind: String,
        /// The chat thread the parking cycle was answering — a desk id for a
        /// channel, a roster agent id for a direct message.
        ///
        /// `None` when no conversation produced it (a workflow delivery, a
        /// scheduler tick, an ambiguous batch), which is also every park
        /// journaled before this existed. Omitted from the wire when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
    /// An operator resolved a parked approval.
    ApprovalResolved {
        /// The approval that was resolved.
        approval_id: ApprovalId,
        /// The operator's verdict.
        verdict: Verdict,
        /// Who resolved it.
        by: Actor,
    },
    /// Feedback was filed against the company.
    FeedbackFiled {
        /// Free-form feedback text.
        note: String,
    },
    /// A payment was received.
    PaymentReceived {
        /// The amount received, in USD.
        amount_usd: f64,
        /// A memo describing the payment.
        memo: String,
    },
    /// The company's lifecycle state changed (e.g. `running` → `paused`),
    /// recorded with the acting actor for the audit trail.
    LifecycleChanged {
        /// The previous lifecycle state.
        from: String,
        /// The new lifecycle state.
        to: String,
        /// Who performed the transition.
        by: Actor,
    },
    /// The governance kill switch was pulled or released (issue #86).
    ///
    /// Separate from [`LifecycleChanged`](Self::LifecycleChanged) because the
    /// two states are orthogonal: emergency stop leaves `lifecycle` alone so
    /// chat keeps working. Folding it into the lifecycle event would make the
    /// audit trail claim a transition that never happened.
    ///
    /// The **durable state**, not just an audit line: the last such event in a
    /// company's log is what the boot replay reads back to decide whether to
    /// come up stopped. There is deliberately no `CompanyRecord` field beside
    /// it, because a second copy of a safety flag is a second thing that can
    /// disagree with the first.
    ///
    /// Written *after* the in-memory flag on engage and *before* it on release,
    /// so whichever of the two writes lands alone leaves the company stopped.
    /// See `CompanyRuntime::emergency_pause`.
    EmergencyPauseChanged {
        /// `true` when the stop was engaged, `false` when it was released.
        engaged: bool,
        /// The operator who did it.
        by: Actor,
        /// Their free-text note, when one was given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// An agent replied in a desk/chat. Journaled by the harness/chat layer so
    /// the GraphQL `Chat.history` resolver (WS2c) can read replies back
    /// alongside the operator messages that prompted them.
    AgentReply {
        /// The desk / group-chat the reply belongs to.
        chat_id: String,
        /// The agent that produced the reply.
        agent_id: String,
        /// The reply text.
        text: String,
        /// The scrubbed processing steps behind this reply — the same
        /// per-bubble [`TurnStep`] timeline the live turn streams and the POST
        /// `/chat` body carries — persisted here so a desk history reload
        /// rehydrates the tool-call timeline, not just the text. Additive:
        /// omitted-when-empty on the wire, so every prior log (and every
        /// non-harness reply, which folds no steps) round-trips byte-identical.
        /// Never carries raw tool arguments, output, or call ids — only the
        /// scrubbed shape (see [`crate::harness::steps`]).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        steps: Vec<TurnStep>,
        /// The board task this reply was produced by, when it came out of a
        /// [`TaskDispatched`](Self::TaskDispatched) cycle rather than a chat
        /// turn (issue #185).
        ///
        /// This is the correlation key the per-task timeline filters on: the
        /// journal is company-scoped, so without it a dispatch's reply cannot
        /// be told apart from every other desk reply in the log.
        ///
        /// `None` for an ordinary chat reply and for every event journaled
        /// before this field existed. Additive in exactly the same way as
        /// [`OperatorMessage`](Self::OperatorMessage)'s `by` / `chat`:
        /// `#[serde(default)]` is what lets an already-persisted log load, and
        /// `skip_serializing_if` is what keeps an untagged reply serializing
        /// byte-for-byte as it did before this field existed, so no stored
        /// record needs migrating.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        /// The message this reply belongs under, as that message's own sequence
        /// position (issue #364).
        ///
        /// Set to the **operator message's** parent, not to the operator
        /// message itself: a reply typed inside a thread and the answer it
        /// draws are two halves of one exchange, and both belong under the row
        /// the thread hangs off. Pointing the answer at the question would nest
        /// a thread inside a thread, which the transcript has no way to render.
        ///
        /// `None` for a reply in the channel itself and for every reply
        /// journaled before this field existed. Additive on the same terms as
        /// `task_id` above.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<EventSeq>,
    },
    /// A reaction was set or cleared on one chat message (issue #364).
    ///
    /// **Per-user rows, event-sourced.** A reaction is a fact about a person —
    /// "who reacted" is most of what a reaction is for — so the durable record
    /// is one line per person per emoji rather than a count, and the count is
    /// derived on read. A count could not answer "did I already react?" for
    /// anyone but the last writer, and a mutable tally on an append-only log
    /// would have to be rewritten in place, which this log cannot do.
    ///
    /// `on` is explicit rather than implied-toggle so the write is
    /// **idempotent**: a retried request, a double tap, or two consoles racing
    /// converge on the state the caller asked for instead of flipping twice.
    /// Folding keeps the last event per `(message, actor, emoji)`.
    ReactionToggled {
        /// The message reacted to, by its sequence position — the same id
        /// `chat/history` returns for that message.
        ///
        /// Deliberately not validated as still-existing on read: the log is
        /// append-only, so a message named here was real when the reaction was
        /// made. A reaction whose message is not in the desk being read simply
        /// folds into nothing.
        message_seq: EventSeq,
        /// The emoji, as the console sent it. Length-bounded and rejected for
        /// control characters at the route, so a journal line can never carry a
        /// blob or a newline dressed as a reaction.
        emoji: String,
        /// Whether the reaction is now set (`true`) or cleared (`false`).
        on: bool,
        /// Who reacted. `None` for a machine/platform credential, which has no
        /// person behind it — read back as "operator", exactly as
        /// `OperatorMessage`'s `by` is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// The Operator deleted a durable memory fact. Journaled for the audit trail
    /// per the Operator-rights section of `docs/spec/company-brain/memory.md`.
    MemoryFactDeleted {
        /// The id of the deleted fact.
        fact_id: String,
    },
    /// An admin changed what this company's agents reach third parties through
    /// (issue #403): the Composio credential the company presents, or a
    /// provider connection authorized under it.
    ///
    /// Journaled for the audit trail, the same reason
    /// [`MemoryFactDeleted`](Self::MemoryFactDeleted) is — a company's tool
    /// access is one of the few things about it that can be changed without
    /// leaving any other mark, so "how did we come to be connected through
    /// *that* account" has to be answerable afterwards.
    ///
    /// **Deliberately carries no credential and no URL.** A journal line is
    /// append-only, exported, and read by the operator projection; the token
    /// is write-only over the whole API and does not stop being so here. A
    /// stable wire word, an optional toolkit slug, and the person — nothing
    /// else.
    ToolAccessChanged {
        /// What changed, as a stable wire word: `credential_set`,
        /// `credential_cleared`, `company_key_set`, `company_key_cleared`, or
        /// `provider_authorization_started`.
        ///
        /// The `credential_*` pair is the **Composio** token
        /// ([`ops::composio`](crate::server::ops::composio)); the `company_key_*`
        /// pair is the company's own TinyHumans identity
        /// ([`ops::company_key`](crate::server::ops::company_key), issue #586).
        /// Two vocabularies rather than one because the changes have different
        /// blast radii — swapping a Composio token repoints one integration,
        /// rotating the company key repoints every surface wired to it — and an
        /// audit reader has to be able to tell them apart.
        ///
        /// The last is deliberately *started*, not *completed*: Composio runs
        /// the OAuth on its own side with no callback here, so all this host
        /// witnesses is that an admin asked for a connect URL for that
        /// toolkit. Naming it a completed connection would put a claim in the
        /// audit trail that nothing verified.
        change: String,
        /// The toolkit slug for a provider authorization (`gmail`, `slack`, …).
        /// `None` for a credential change, which is not per-provider.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        toolkit: Option<String>,
        /// Who made the change. The routes that emit this require an admin
        /// session, so in practice it is always `Some` and always a person;
        /// the `Option` matches the
        /// [`WorkflowCreated`](Self::WorkflowCreated) precedent and lets an
        /// already-persisted log load unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// A board task was moved into `in_progress` and dispatched to its assignee
    /// for one agent turn on the embedded runtime. Journaled so the dispatch is
    /// auditable and replayable. Only the `openhuman` `HarnessBrain` acts on it;
    /// the default build's `EchoBrain` ignores it, so the board stays inert
    /// without the harness.
    TaskDispatched {
        /// The id of the dispatched task card.
        task_id: String,
        /// The [`RunRecord`](crate::ports::runs::RunRecord) this dispatch is an
        /// attempt under (issue #242), minted at the dispatch choke point
        /// *before* the cycle is spawned.
        ///
        /// Carrying it on the event is what makes the journal self-describing:
        /// the run row and the durable log line name each other, so a reader
        /// holding either one can find the other without re-deriving identity
        /// from timestamps. It also keeps
        /// [`Brain::run_cycle`](crate::ports::brain::Brain::run_cycle)'s
        /// signature stable — the id rides the event the brain already reads
        /// rather than a new argument every brain would have to thread.
        ///
        /// `None` for a dispatch whose run row could not be minted (record-keeping
        /// never fails the work it records) and for every event journaled before
        /// this field existed. Additive in exactly the way
        /// [`AgentReply`](Self::AgentReply)'s `task_id` is: `#[serde(default)]`
        /// lets an already-persisted log load, and `skip_serializing_if` keeps an
        /// untagged dispatch serializing byte-for-byte as it did before, so no
        /// stored record needs migrating.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },
    /// An agent's MCP tool call failed during a turn, journaled by the harness
    /// so the operator has an audit trail of which server/tool broke and why.
    /// The `message` is always **scrubbed** at the source (the
    /// `OcMcpCallTool` → `HarnessBrain` drain path), so this record can never
    /// carry a credential, response body, or URL query string. Additive: old
    /// logs never contain it, and its presence doesn't change how any existing
    /// variant serializes (same `by`/`chat` precedent).
    McpCallFailed {
        /// The MCP server the failing call targeted.
        server: String,
        /// The remote tool the agent tried to call.
        tool: String,
        /// A stable status code (e.g. `credential_required`, `tool_call_rejected`).
        status: String,
        /// A short, scrubbed, operator-facing message.
        message: String,
        /// The board task whose dispatch turn made the failing call, when the
        /// failure happened inside a [`TaskDispatched`](Self::TaskDispatched)
        /// cycle (issue #185). Lets a task's failed tool calls be filtered out
        /// of the company-scoped journal onto its own timeline.
        ///
        /// `None` for a failure raised during a chat turn and for every event
        /// journaled before this field existed. Same additive contract as
        /// [`AgentReply`](Self::AgentReply)'s `task_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    /// A new workflow graph was authored and enabled (issue #112), from either
    /// the console `POST …/workflows` route or the orchestrator's
    /// `create_workflow` tool. Journaled best-effort **after** the graph is
    /// persisted and enabled, so it records a completed create — a journal
    /// failure never rolls the create back. Additive: old logs never carry it,
    /// and its presence doesn't change how any existing variant serializes.
    WorkflowCreated {
        /// The new workflow's id (its `workflows/<id>.toml` stem).
        workflow_id: String,
        /// The new workflow's display name.
        name: String,
        /// Who authored it, when known. `None` when created by a surface that
        /// carries no attributed actor (the current create paths); kept as an
        /// `Option` so a future attributed create needs no migration, mirroring
        /// [`OperatorMessage`](Self::OperatorMessage)'s `by`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// An existing workflow graph was replaced wholesale (issue #259), from the
    /// console's `PUT …/workflows/{wid}` route. Journaled best-effort **after**
    /// the new body is persisted, so it records a completed edit — a journal
    /// failure never rolls the update back. Additive, same contract as
    /// [`WorkflowCreated`](Self::WorkflowCreated).
    ///
    /// **The graph body is deliberately NOT carried.** A company's journal is
    /// one append-only log shared by chat, audit and run history, and it is read
    /// by the operator SSE projection and wired to the inference sidecar — a
    /// TOML body on it would put the graph's full contents (agent prompts,
    /// destination addresses) somewhere none of those readers need it. The id
    /// and name are what an audit reader needs; the body is read from the record.
    WorkflowUpdated {
        /// The edited workflow's id. Never changes across an update — a rename
        /// through `PUT` is rejected, because the id keys the union read path,
        /// the scheduler and the run history.
        workflow_id: String,
        /// The workflow's display name **after** the edit (the name may change
        /// even though the id may not).
        name: String,
        /// Who edited it, when known. `None` from the current unattributed
        /// surfaces; same forward-compatible shape as
        /// [`WorkflowCreated`](Self::WorkflowCreated)'s `by`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// A workflow graph was removed (issue #259), from the console's
    /// `DELETE …/workflows/{wid}` route. Journaled best-effort **after** the
    /// overlay body and the manifest-enabled id are both gone, so it records a
    /// completed delete. Additive, same contract as
    /// [`WorkflowCreated`](Self::WorkflowCreated).
    ///
    /// Past [`WorkflowRunFinished`](Self::WorkflowRunFinished) entries for this
    /// id are deliberately left in place — the journal is append-only, and what
    /// a workflow *did* stays true after the workflow is gone. `GET
    /// …/workflows/runs` keeps serving them.
    WorkflowDeleted {
        /// The removed workflow's id.
        workflow_id: String,
        /// Its display name at the moment it was removed, so a journal reader
        /// need not resolve an id that no longer exists.
        name: String,
        /// Who removed it, when known. `None` from the current unattributed
        /// surfaces.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// A workflow was switched on or off (issue #276) — from the console's
    /// `PUT …/workflows/{wid}/enabled` route, or from the disarm rule that
    /// forces `false` when a create or an edit arms a schedule. Journaled
    /// best-effort **after** the flag is persisted, so it records a completed
    /// change. Additive, same contract as
    /// [`WorkflowCreated`](Self::WorkflowCreated).
    ///
    /// **Only a real transition is journaled.** Toggling a workflow to the state
    /// it already holds writes nothing, so this log answers "when did this stop
    /// firing, and was it a person or the disarm rule" rather than counting
    /// clicks.
    WorkflowEnabledChanged {
        /// The workflow's id.
        workflow_id: String,
        /// Its display name at the moment of the change, so an audit reader need
        /// not resolve the id against a graph that may since have been edited.
        name: String,
        /// The state it moved **to**: `true` armed, `false` paused.
        enabled: bool,
        /// Why it moved. See [`WorkflowEnabledReason`].
        reason: WorkflowEnabledReason,
        /// Who changed it, when known. `None` from the current unattributed
        /// surfaces, and always `None` for a [`Disarmed`](WorkflowEnabledReason::Disarmed)
        /// entry — that one is the host's rule firing, not a person's decision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// An operator steered an in-flight run — paused, cancelled, or redirected a
    /// dispatched task (or cancelled a delegation) from chat (issue #111).
    /// Journaled best-effort **after** the steer is accepted by the in-flight
    /// registry, so it records an accepted operator control action. Additive: old
    /// logs never carry it, and its presence doesn't change how any existing
    /// variant serializes (same `by` / skip-if-none precedent as
    /// [`WorkflowCreated`](Self::WorkflowCreated)).
    TaskSteered {
        /// The steered run's key — the board task id, or a delegation run id.
        task_id: String,
        /// The action taken, as a stable wire word: `pause` / `cancel` /
        /// `redirect`.
        action: String,
        /// The operator's redirect instruction (codepoint-capped), present only
        /// on a `redirect`. Omitted from the wire otherwise, and — being
        /// operator-authored free text — never projected onto the SSE stream.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instruction: Option<String>,
        /// Who steered, when known. `None` on a surface that carries no attributed
        /// actor; kept `Option` so a future attributed steer needs no migration,
        /// mirroring [`OperatorMessage`](Self::OperatorMessage)'s `by`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// A board card was written (issue #464) — opened, changed, or removed.
    ///
    /// The board's *announcement*, and the thing that was missing: every other
    /// task event here describes a card that already exists
    /// ([`TaskDispatched`](Self::TaskDispatched) fires on the `in_progress`
    /// edge, [`DeskTaskCompleted`](Self::DeskTaskCompleted) on the settle), so
    /// nothing said a card had come into being. A card opened from chat intake,
    /// from a delegation, or from the publish drain left no trace on the feed at
    /// all, and a console watching the board had nothing to react to.
    ///
    /// **Emitted from the store, not from the callers.** It is appended by the
    /// [`BoardAnnouncer`](crate::runtime::BoardAnnouncer) decorator wrapping the
    /// company's [`TaskStore`](crate::ports::tasks::TaskStore), which is the one
    /// place every writer already passes through. Emitting it per call site
    /// would have meant one arm per creation path — and the next path added
    /// would silently have none, which is the shape of the bug this fixes.
    ///
    /// **A record, never a stimulus.** It is appended after the write it
    /// describes and is never fed into a cycle; the cycle's own trigger
    /// classification treats it as a pass-through, like every other record.
    /// Announcing a write must not start work — a card that opened a cycle by
    /// existing would re-enter this same store and announce again.
    ///
    /// Additive: old logs never carry it, and its presence doesn't change how
    /// any existing variant serializes.
    TaskCardChanged {
        /// The written card's id.
        task_id: String,
        /// What happened to it, as a stable wire word: `opened` (the write
        /// brought the card into existence), `updated` (it changed an existing
        /// one), or `removed` (it was deleted).
        ///
        /// A word rather than a `bool`, following
        /// [`ToolAccessChanged`](Self::ToolAccessChanged)'s `change`: there are
        /// three outcomes, not two, and a flag would have had to grow a second
        /// one to say a card is gone.
        change: String,
        /// The column the card sits in after the write. `None` on a `removed`
        /// card, which is no longer in one — omitted rather than an empty
        /// string, so "gone" cannot be mistaken for a column whose id is blank.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<String>,
    },
    /// A workspace node was written (issue #327) — created, changed, or
    /// removed.
    ///
    /// [`TaskCardChanged`](Self::TaskCardChanged)'s counterpart for the note
    /// tree, and missing for exactly the same reason: nothing on the feed said
    /// the workspace had moved, so a console with the Workspace tab open saw an
    /// agent's write only on a manual refresh or a window refocus. Now that
    /// agents create notes (#551) and published deliverables land in the tree
    /// (#552), that stale window is where most of the tree's activity happens.
    ///
    /// **Emitted from the store, not from the callers.** Appended by the
    /// [`WorkspaceAnnouncer`](crate::runtime::WorkspaceAnnouncer) decorator
    /// wrapping the company's
    /// [`WorkspaceStore`](crate::ports::workspace::WorkspaceStore) — the one
    /// place the seeder, the console routes, the agent tools and the publish
    /// drain all pass through. An emit per call site is the shape of the bug:
    /// correct only for the paths somebody remembered.
    ///
    /// **A record, never a stimulus.** Appended after the write it describes and
    /// never fed into a cycle. A note that started work by existing would
    /// re-enter this store and announce again.
    ///
    /// Additive: old logs never carry it, and its presence doesn't change how
    /// any existing variant serializes.
    WorkspaceChanged {
        /// The written node's id.
        node_id: String,
        /// What happened to it, as a stable wire word: `opened` (the write
        /// brought the node into existence), `updated` (its body or its place
        /// in the tree changed), or `removed` (it was deleted).
        ///
        /// The same three words [`TaskCardChanged`](Self::TaskCardChanged)
        /// uses, deliberately — a console that already knows how to read one
        /// change vocabulary should not have to learn a second.
        change: String,
    },
    /// A dispatched board task finished its run (issue #185) — the terminal
    /// anchor a per-task timeline ends on and a lineage rollup counts.
    ///
    /// Journaled by the harness at the end of a
    /// [`TaskDispatched`](Self::TaskDispatched) cycle, **after** the card's
    /// landing column has been persisted, so it always records a completed
    /// run. "Completed" here means *the run stopped*, not *it succeeded*: a
    /// cancelled, paused, or failed dispatch emits one too, and `column`
    /// carries where the card actually landed. Without that, a timeline could
    /// not distinguish "still running" from "finished badly".
    ///
    /// This issue only *adds* the event. #171's done-transition can consume
    /// it; nothing here writes the board column off the back of it.
    ///
    /// Additive: old logs never carry it, and its presence doesn't change how
    /// any existing variant serializes.
    DeskTaskCompleted {
        /// The completed task card's id.
        task_id: String,
        /// The desk / agent that ran it — the resolved responder, not the
        /// card's raw `assignee` (which may name nobody on the roster).
        desk: String,
        /// The run's operator-facing result text.
        ///
        /// This is the agent's own reply (or a short `dispatch failed: …` /
        /// cancellation line), which is the same text already written into the
        /// card's note — never raw tool output, arguments, or call ids.
        output: String,
        /// The board column the card landed in: `in_review` on a normal
        /// finish, `todo` on a failure or cancellation, `paused` on a
        /// pause. Lets a reader tell a successful run from a stopped one
        /// without re-deriving it from `output`.
        column: String,
        /// The artifacts this run published (issue #244), by id — empty when it
        /// published nothing, which is the common and entirely legitimate case.
        ///
        /// The terminal anchor is where a reader asks *"what did this task
        /// produce?"*, and before #244 the only answer available was `output`,
        /// which is the chat reply and not a deliverable. Carrying the ids here
        /// means a card in a terminal column can link to what it actually made
        /// without a second query against the artifact store.
        ///
        /// Additive: `#[serde(default)]` so every journal line written before
        /// this field existed still replays, and skipped when empty so the
        /// no-deliverable case adds nothing to the log.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        artifact_ids: Vec<String>,
        /// The conversation the card was raised from (issue #377), when one
        /// was — [`TaskRecord::origin_chat_id`](crate::ports::tasks::TaskRecord::origin_chat_id),
        /// stamped here at the moment the run settles.
        ///
        /// **Captured, never derived.** [`desk`](Self::DeskTaskCompleted::desk)
        /// is the *responder* — an agent id like `engineer` — and a channel is
        /// a desk id like `engineering`, so no reader can recover the origin
        /// from the fields that were already here. Deriving it at completion
        /// time would also re-open issue #435's failure mode: two places
        /// deciding "which conversation is this?" by different rules. The card
        /// has recorded its origin since #151, on every conversational path
        /// that opens one, so the terminal simply carries what the card already
        /// knows.
        ///
        /// `None` is a **positive fact**, not a lost id: nobody raised this
        /// card from a conversation (it was created on the board, by a
        /// scheduler, or before #151). Such a card belongs to no channel and
        /// `chat_history::owns` deliberately keeps it out of every one of them
        /// — it is emphatically *not* folded into the General desk.
        ///
        /// Additive: `#[serde(default)]` so every journal line written before
        /// this field existed still replays, and skipped when absent so a
        /// board-created card adds nothing to the log.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_chat_id: Option<String>,
    },
    /// A human posted to a task's discussion thread (issue #335).
    ///
    /// The per-task Discussion tab's whole backing store. A task discussion is
    /// its *own* thread — not a filtered view of the company chat — but it is
    /// not a second message store either: it lives in this journal, beside the
    /// events the same task's timeline is folded from, so the two notions of
    /// "something happened on this task" cannot drift apart. `GET
    /// …/tasks/{task_id}` projects both out of one traversal.
    ///
    /// Operator-authored only in v1. Nothing dispatches an agent turn off the
    /// back of it and no agent reads it: posting is a durable note on the card,
    /// not a delegation surface. That is a product decision rather than a
    /// technical limit — see `docs/modules/server/README.md` — and the wire
    /// projections here reflect it: the text is deliberately never forwarded to
    /// the inference sidecar or the SSE stream.
    ///
    /// Append-only, like every other variant: this event is never edited and
    /// never removed. What #358 added is not a mutation of it but a *successor*
    /// — see [`TaskDiscussionRedacted`](Self::TaskDiscussionRedacted) — so the
    /// fact that something was said, by whom and when, stays in the record even
    /// once its text stops being readable.
    ///
    /// Additive: old logs never carry it, and its presence doesn't change how
    /// any existing variant serializes.
    TaskDiscussionPosted {
        /// The board card the message belongs to.
        task_id: String,
        /// The message text, codepoint-capped at the route boundary
        /// ([`MAX_DISCUSSION_CHARS`](crate::ports::tasks::MAX_DISCUSSION_CHARS)).
        text: String,
        /// Who posted, when a signed-in human is behind the request.
        ///
        /// `None` for a machine credential, which has no person to name and
        /// reads back as "operator" — the same fallback
        /// [`OperatorMessage`](Self::OperatorMessage)'s `by` takes, and the same
        /// additive `skip_serializing_if` contract.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// A discussion post's text was withdrawn (issue #358).
    ///
    /// ## Why this exists
    ///
    /// A task discussion is the one surface where a person types free prose
    /// about work that is *blocked*, and the next thing pasted into a thread
    /// like that is often the thing that unblocks it — a key, a token, a
    /// customer record. #348's own fixture reads "blocked on the API key". Until
    /// this event there was no way to take it back: no edit, no delete, no
    /// tombstone, and because the journal is what export/import ships, the
    /// message was not merely permanent but **portable**.
    ///
    /// ## Why a successor rather than a mutation
    ///
    /// The log is append-only and that property is load-bearing well beyond
    /// this tab: no control alters a past event, sequence numbers are stable
    /// ids that threads and reactions name, and import replays a bundle from
    /// zero to reproduce them. Rewriting or dropping the post in place would
    /// break all three. So the post stays exactly as journaled and this event
    /// **supersedes** it: every reader folds the pair and shows the post's
    /// existence, author and time with its text replaced.
    ///
    /// ## What it does NOT claim
    ///
    /// This is not an at-rest erasure of the original bytes on the instance
    /// that holds them, and it must not be read as one — the append-only
    /// property is precisely what forbids that. What it guarantees is that the
    /// text stops being served by any read surface and stops leaving the
    /// building: [`export`](crate::store::export) replaces the superseded text
    /// before the bundle is written, so a round trip cannot resurrect it. A
    /// leaked credential still has to be rotated; this stops the record of it
    /// being readable by every member of the company and travelling with the
    /// bundle.
    ///
    /// Scoped deliberately to the discussion. `OperatorMessage` has the same
    /// shape of problem and is **not** covered here; see
    /// `docs/modules/server/README.md` for why that is a separate decision
    /// rather than an oversight.
    ///
    /// Additive: old logs never carry it, and its presence doesn't change how
    /// any existing variant serializes.
    TaskDiscussionRedacted {
        /// The card the superseded post belongs to. Carried so a fold that is
        /// already filtering one task's journal can skip a tombstone for
        /// another without resolving the post it names.
        task_id: String,
        /// The sequence position of the
        /// [`TaskDiscussionPosted`](Self::TaskDiscussionPosted) this supersedes.
        ///
        /// Stable across export→import: a bundle replays from zero, so the
        /// referenced position survives the round trip that carries both events.
        seq: u64,
        /// Who withdrew it, when a signed-in human is behind the request.
        ///
        /// Present for the same reason the post's `by` is: a message that
        /// disappears with nobody's name on it is one a member can quietly
        /// remove from a thread others were reading. `None` for a machine
        /// credential, which reads back as "operator".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// A workflow run finished (issue #228) — the durable record of what a run
    /// actually did, journaled from **both** entry points: the console's Run
    /// button and the cron [`WorkflowScheduler`](crate::runtime::WorkflowScheduler).
    ///
    /// Before this, a run's outcome existed only in the moment. A manual run's
    /// [`DeliveryReport`] rows lived in the console drawer until it was
    /// dismissed; a scheduled run's reached only host stdout, which on a hosted
    /// tenant is the platform team rather than the tenant's operator. Nothing
    /// wrote a run outcome anywhere the console could read it back — so a report
    /// that did not leave the building was unfindable an hour later.
    ///
    /// This is **not** [issue #242's `RunRecord`](crate::ports::types) and does
    /// not replace it. That record is a *task-attempt* record minted at the task
    /// dispatch choke point, keyed to a board task with an attempt ordinal. A
    /// workflow run enters through a different port entirely
    /// ([`WorkflowRunner::run`](crate::ports::WorkflowRunner::run)) and produces
    /// host-side delivery rows per output node; it has no task and no attempt
    /// ordinal to be keyed on.
    ///
    /// Journaled **best-effort, after** the run returns, so it always records a
    /// finished run and an append failure never disturbs the run path.
    ///
    /// Additive: old logs never carry it, and its presence doesn't change how
    /// any existing variant serializes. Every optional/collection field carries
    /// `#[serde(default)]` + `skip_serializing_if`, the same contract as
    /// [`AgentReply`](Self::AgentReply)'s `task_id`, so a journal written before
    /// this variant existed still loads and every already-persisted event stays
    /// byte-identical.
    WorkflowRunFinished {
        /// The workflow graph that ran (its `workflows/<id>.toml` stem).
        workflow_id: String,
        /// Whether a cron schedule started this run rather than an operator.
        /// The distinction is the point: a scheduled run is the
        /// nobody-was-watching case this event exists for.
        scheduled: bool,
        /// A correlation id for the run — the same id its
        /// [`WorkflowRunStarted`](Self::WorkflowRunStarted) and per-node events
        /// carry. Every current entry point mints one and populates it here;
        /// kept `Option` so a legacy record written before entry points minted
        /// one, and any future entry point with no id to give, need no
        /// migration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        /// One row per attempt to route a reached `output` node's report to its
        /// destination — the same rows a manual run hands back in its HTTP
        /// response. Empty for a graph that routes nothing.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        deliveries: Vec<DeliveryReport>,
        /// Node ids the run left waiting on a human approval.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pending_approvals: Vec<String>,
        /// The error that ended the run, when it failed outright rather than
        /// finishing with rows.
        ///
        /// This is the field that closes the loudest hole: today a scheduled
        /// run's `Err` arm only warns to host stdout, so **the worst outcome is
        /// currently the quietest**. `None` on a run that completed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Whether an operator stopped this run (issue #383) rather than it
        /// finishing or failing.
        ///
        /// Deliberately **not** folded into [`error`](Self::WorkflowRunFinished):
        /// a cancelled run carries no error at all, because nothing went wrong.
        /// Three terminal readings now exist and each has to stay legible on its
        /// own — a run that *failed* (an `error` naming a node), one
        /// *interrupted by a host restart* (the boot sweep's synthetic error),
        /// and one *stopped by an operator* (this flag, no error). Collapsing
        /// any pair of them would put a deliberate stop in the failure count.
        ///
        /// Additive and replay-safe: `#[serde(default)]` decodes every row
        /// written before #383 as `false`, and `skip_serializing_if` keeps a
        /// non-cancelled run's line byte-identical to what it was.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cancelled: bool,
        /// System notices raised about this run (issue #638) — today, that a
        /// node's turn gated more tool calls than the per-batch cap allows and
        /// the excess was discarded.
        ///
        /// The run-side counterpart of the operator bubble the chat path pushes
        /// (#561). A run has no conversation to speak on, so without this the
        /// operator sees the first `cap` cards on the Approvals page and no
        /// indication that any more were gated.
        ///
        /// Separate from [`error`](Self::WorkflowRunFinished::error) on purpose:
        /// a run that overflowed the cap **succeeded**, and putting this there
        /// would mark it failed and inflate the failure count.
        ///
        /// `#[serde(default)]` + `skip_serializing_if` so a pre-#638 line
        /// replays and an ordinary run's event serializes byte-for-byte as it
        /// did before — which is every run that did not overflow.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notices: Vec<String>,
        /// One row per board write this run's agent nodes performed (issue #661
        /// / M5) — the same rows the synchronous run response hands back.
        ///
        /// This is what makes a **scheduled** run's board writes readable at
        /// all: nobody was watching the response, so without this the only
        /// evidence a 3am run opened a card is the card itself, with nothing
        /// saying which run put it there.
        ///
        /// Rides the `Ok` arm only, like
        /// [`cancelled`](Self::WorkflowRunFinished) and
        /// [`notices`](Self::WorkflowRunFinished): a run that returned nothing
        /// carries no rows here. **The cards themselves are unaffected** — a
        /// board write is durable the moment the drain performs it, so a run
        /// that later failed outright still leaves every card it opened on the
        /// board. What an `Err` loses is the row *listing* them, not the work.
        ///
        /// `#[serde(default)]` + `skip_serializing_if` so a line written before
        /// this field existed replays, and a run that touched no card
        /// serializes byte-for-byte as it did before — which is nearly all of
        /// them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        board: Vec<WorkflowRunBoardRow>,
        /// One row per node this run blocked on a human (issue #881) — the
        /// same rows the synchronous run response hands back.
        ///
        /// What makes a **scheduled** run's blockage readable at all: nobody
        /// was watching the response, so without this the only evidence a 3am
        /// run stopped short is an approval card nothing ties back to a run.
        ///
        /// `#[serde(default)]` + `skip_serializing_if` so a line written
        /// before this field existed replays — the event is folded at boot, so
        /// a field without a default would make every pre-existing journal
        /// line fail to parse, which is silent history loss rather than a
        /// compile error — and a run that blocked on nobody serializes
        /// byte-for-byte as it did before.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocked_nodes: Vec<WorkflowBlockedNode>,
        /// One row per approval this run parked (issue #880) — a receipt of
        /// what it opened, not a snapshot of what is still outstanding.
        ///
        /// The field the three `feature_pipeline` runs needed: they parked
        /// fifteen `publish_artifact` cards between them and their run records
        /// said nothing, because `pendingApprovals` means the engine's gate
        /// nodes and `deliveries` means `output`-node routing. Both were
        /// honest; neither was an answer.
        ///
        /// Same `#[serde(default)]` + `skip_serializing_if` contract as
        /// `blocked_nodes` above, and for the same replay reason.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        approvals: Vec<WorkflowRunApprovalRow>,
    },
    /// A workflow run began (issue #371) — the opening bracket of a run's
    /// per-node progress trail.
    ///
    /// Before this, a run reported once, at the end. Between pressing Run and
    /// the drawer appearing there was no signal at all: a long run was
    /// indistinguishable from a wedged one, and a run that died at the fourth of
    /// six nodes said only that it died. This event and
    /// [`WorkflowNodeFinished`](Self::WorkflowNodeFinished) are the trail, and
    /// they ride the journal rather than a side channel because the journal
    /// already feeds the live operator SSE projection — **one append serves both
    /// the live half and the durable half**, so a scheduled run nobody watched
    /// reads back exactly as a watched one did.
    ///
    /// This is deliberately **not** [issue #242's `RunRecord`](Self::TaskDispatched).
    /// That record keys to a board task with an attempt ordinal; a workflow run
    /// has neither, and minting a synthetic task id to borrow the shape would
    /// leak a lie into every `RunStore` consumer.
    ///
    /// [`run_id`](Self::WorkflowRunStarted::run_id) is **required** here (unlike
    /// on `WorkflowRunFinished`, where it stays optional for the rows written
    /// before entry points minted one). It is the correlation key: a run's node
    /// events and its finished event share it, so the fold can group them and
    /// the console can overlay one past run's states onto the canvas.
    ///
    /// Additive: old journals never carry it, and its presence changes how no
    /// existing variant serializes.
    WorkflowRunStarted {
        /// The workflow graph that is running (its `workflows/<id>.toml` stem).
        workflow_id: String,
        /// The run's correlation id, minted by the entry point.
        run_id: String,
        /// Whether a cron schedule started this run rather than an operator.
        scheduled: bool,
    },
    /// One non-trigger node of a workflow run began executing (issue #382),
    /// reported by the engine's `RunObserver` immediately before the node's
    /// first attempt.
    ///
    /// The opening bracket of a node the way
    /// [`WorkflowNodeStarted`](Self::WorkflowNodeStarted)'s run-level sibling
    /// [`WorkflowRunStarted`](Self::WorkflowRunStarted) is the bracket of a run:
    /// it turns "which node is executing right now" from a client-side guess
    /// (derived from graph topology) into a fact the engine reports. It is
    /// emitted on the **same** unbounded channel as this node's
    /// [`WorkflowNodeFinished`](Self::WorkflowNodeFinished) and always ahead of
    /// it, so a reader folding the journal sees a node light up before it settles.
    ///
    /// **Structural ids only.** Unlike `WorkflowNodeFinished`, this carries no
    /// status and no duration — the node has not run yet — and, like every event
    /// on the progress trail, no node input. There is nothing here to scrub: a
    /// node id is the whole payload, so the same operator-SSE / inference-sidecar
    /// readers see only "node X of run R started".
    ///
    /// Additive: an entirely new `kind`, so no journal written before it existed
    /// carries it, and its presence changes how no existing variant serializes.
    WorkflowNodeStarted {
        /// The workflow graph that is running.
        workflow_id: String,
        /// The run this node belongs to — the same id its
        /// [`WorkflowRunStarted`](Self::WorkflowRunStarted) and
        /// [`WorkflowNodeFinished`](Self::WorkflowNodeFinished) carry.
        run_id: String,
        /// The graph node that just started executing.
        node_id: String,
    },
    /// One non-trigger node of a workflow run finished (issue #371), reported by
    /// the engine's `RunObserver` as the graph is walked.
    ///
    /// One event per node — roughly eight for a six-node graph, against the
    /// dozens of steps a single chat turn emits — which is why the journal is
    /// the right carrier at this volume rather than a dedicated store.
    ///
    /// **No node output and no error text ride this event.** That is the same
    /// scrubbing stance the live turn-progress frames take: the journal is read
    /// by the operator SSE projection and wired to the inference sidecar, and a
    /// node's raw items are exactly the payload none of those readers need. The
    /// run-level failure reason already lands on
    /// [`WorkflowRunFinished::error`](Self::WorkflowRunFinished) — a tenant-scoped
    /// surface — so nothing is lost by keeping this one structural.
    ///
    /// Each node's matching *started* bracket is
    /// [`WorkflowNodeStarted`](Self::WorkflowNodeStarted) (issue #382), emitted
    /// on the same channel just before the node's first attempt. Before #382 the
    /// engine's `RunObserver` had only `on_step_finish`, so "currently executing"
    /// had to be derived client-side from the graph topology the console holds;
    /// the engine now reports the start directly, so that derivation is gone.
    WorkflowNodeFinished {
        /// The workflow graph that is running.
        workflow_id: String,
        /// The run this node belongs to — the same id its
        /// [`WorkflowRunStarted`](Self::WorkflowRunStarted) carries.
        run_id: String,
        /// The graph node that just finished.
        node_id: String,
        /// Whether the node succeeded or errored.
        status: WorkflowNodeStatus,
        /// Wall-clock duration of the node's execution, in milliseconds.
        elapsed_ms: u64,
        /// The node's non-fatal data-binding diagnostics (issue #1014): the
        /// config path of every `=`-expression that resolved to `null` during
        /// this node's execution — the engine's own list of broken wiring (see
        /// `crate::ports::WorkflowRunNodeRow::diagnostics`).
        ///
        /// **Config paths only, no node output.** A null resolution carries no
        /// value, and only its config *location* rides here — the same scrubbing
        /// stance the rest of this event takes, so the operator-SSE projection
        /// and the inference sidecar see the broken wiring's address and never a
        /// payload.
        ///
        /// `#[serde(default)]` + `skip_serializing_if` so a journal line written
        /// before this field existed folds back with an empty list — the event
        /// is replayed at boot, and a field without a default would make every
        /// pre-existing line fail to parse (silent history loss rather than a
        /// compile error) — and a node with no unresolved wiring serializes
        /// byte-for-byte as it did before.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        diagnostics: Vec<String>,
    },
    /// One `output` node's report actually left the process (issue #529) — the
    /// durable record of a dispatch that the run's own
    /// [`WorkflowRunFinished`](Self::WorkflowRunFinished) does not carry across a
    /// crash.
    ///
    /// # The hole this closes
    ///
    /// A run's delivery rows live only on
    /// [`WorkflowRunFinished::deliveries`](Self::WorkflowRunFinished), which is
    /// journaled **after** the run returns. A crash, panic, or mid-graph failure
    /// therefore orphans the side effect: the mail left the process, but the
    /// boot sweep settles the run `FAILED` with no delivery record, and the
    /// operator's re-run re-delivers every already-sent report to real people
    /// (issue #438's ledger rides one approval lineage's trigger input and is not
    /// persisted, so an independently re-run workflow re-delivers). This event is
    /// written **write-behind, at dispatch** — immediately after each `Sent` send
    /// and each `Pending` park — so a durable ledger of what already went out
    /// survives a crash and a re-run can skip it. See
    /// [`delivered_by_unsettled_runs`](crate::runtime::delivered_by_unsettled_runs).
    ///
    /// Write-behind, not write-ahead: a crash *between* the journal and the send
    /// would silently suppress a report, which is worse than one duplicate. So
    /// the record can only ever lag the send, never precede it.
    ///
    /// # Dedupe identity is `node`
    ///
    /// The fold keys on [`node`](Self::WorkflowReportDelivered::node) so it
    /// unions cleanly with #438's
    /// [`DeliveredReport`](crate::runtime::workflow_resume::DeliveredReport),
    /// whose identity is also the node. An `owner` destination that fanned out to
    /// several admins writes one line per recipient, so `target` differs across
    /// lines for one node; per-recipient dedupe is a documented deferred limit,
    /// exactly as #438's ledger is per node rather than per recipient.
    ///
    /// Additive: an entirely new `kind`, so no journal written before it existed
    /// carries it, and its presence changes how no existing variant serializes.
    WorkflowReportDelivered {
        /// The workflow graph that delivered (its `workflows/<id>.toml` stem).
        workflow_id: String,
        /// The run that dispatched this report — the same id its
        /// [`WorkflowRunStarted`](Self::WorkflowRunStarted) carries.
        run_id: String,
        /// The `output` node whose report left the process. This is the dedupe
        /// identity the fold and #438's ledger both key on.
        node: String,
        /// The destination kind as authored (`owner` / `email` / `channel`) — the
        /// description beside the identity, so a reader sees *what* went where.
        ///
        /// Renamed to `destination_kind` on the wire: this enum is
        /// internally-tagged under `kind`, so a field literally named `kind`
        /// would collide with the tag and emit a duplicate key. The Rust name
        /// stays `kind` to read the same as its
        /// [`DeliveryReport`](crate::ports::DeliveryReport) and
        /// [`DeliveredReport`](crate::runtime::workflow_resume::DeliveredReport)
        /// siblings.
        #[serde(rename = "destination_kind")]
        kind: String,
        /// The address or channel actually dispatched to. `None` only when a
        /// destination named none; for `owner` this is the server-resolved
        /// recipient, not something the graph named. Optional on the wire so a
        /// line stays minimal when there is nothing to record.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// A call inside a `sub_workflow` child ran **without** being offered for
    /// approval, on a company whose policy would have parked the same call at
    /// the top level (issue #617).
    ///
    /// # Why this is a journal line and not a fix
    ///
    /// A child graph is resolved and run *inside* the engine, so its nodes never
    /// reach the gate pass that #460 and #614 added. Gating them is not
    /// available: tinyflows cannot resume a child across the sub-workflow
    /// boundary, and a paused child halts the parent with an unresumable error —
    /// so marking the child's node would convert a run that works into one that
    /// fails with no card to decide. Until the engine can resume across that
    /// boundary, the honest thing is to keep running and **say so**.
    ///
    /// That is what this records. An operator auditing what the company was
    /// asked to approve would otherwise see a hole with nothing explaining it;
    /// this turns the hole into a line naming the child, the node and the call.
    /// It is an audit record of a known limitation, not a decision anybody made.
    ///
    /// Journaled best-effort at child *resolution*, so it lands whether or not
    /// the child's node is ultimately reached — the point is that the call was
    /// never offered, and a run that stops earlier for another reason does not
    /// make that less true. Deliberately **not** written for a dry run, which
    /// executes nothing.
    ///
    /// Additive: an entirely new `kind`, so no journal written before it existed
    /// carries it, and its presence changes how no existing variant serializes.
    WorkflowChildCallNotOffered {
        /// The graph the run started from — the one an operator recognises.
        workflow_id: String,
        /// The child graph the call actually sits in. Equal to `workflow_id`
        /// only if a workflow references itself, which the cycle guard rejects.
        child_workflow_id: String,
        /// The run that resolved the child.
        run_id: String,
        /// The node inside the child that makes the call.
        node: String,
        /// The tool it would run — a `tool_call` node's slug, or `http_request`.
        tool: String,
        /// The policy's own words for why this call would have been parked at
        /// the top level, so the line says what the operator was not asked.
        reason: String,
    },
}

impl CompanyEvent {
    /// The variant's serialized discriminant — the same string that appears
    /// under the internally-tagged `kind` field of a journal line.
    ///
    /// Written out rather than derived so the value is available without
    /// serializing, and so a rename of the wire tag has to be made here too
    /// instead of drifting silently.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::OperatorMessage { .. } => "OperatorMessage",
            Self::TurnStarted { .. } => "TurnStarted",
            Self::TurnFailed { .. } => "TurnFailed",
            Self::RunStatusChanged { .. } => "RunStatusChanged",
            Self::WebhookReceived { .. } => "WebhookReceived",
            Self::ScheduleFired { .. } => "ScheduleFired",
            Self::A2aTaskReceived { .. } => "A2aTaskReceived",
            Self::ApprovalParked { .. } => "ApprovalParked",
            Self::ApprovalResolved { .. } => "ApprovalResolved",
            Self::FeedbackFiled { .. } => "FeedbackFiled",
            Self::PaymentReceived { .. } => "PaymentReceived",
            Self::LifecycleChanged { .. } => "LifecycleChanged",
            Self::AgentReply { .. } => "AgentReply",
            Self::ReactionToggled { .. } => "ReactionToggled",
            Self::MemoryFactDeleted { .. } => "MemoryFactDeleted",
            Self::ToolAccessChanged { .. } => "ToolAccessChanged",
            Self::TaskDispatched { .. } => "TaskDispatched",
            Self::McpCallFailed { .. } => "McpCallFailed",
            Self::WorkflowCreated { .. } => "WorkflowCreated",
            Self::WorkflowUpdated { .. } => "WorkflowUpdated",
            Self::WorkflowDeleted { .. } => "WorkflowDeleted",
            Self::TaskSteered { .. } => "TaskSteered",
            Self::TaskCardChanged { .. } => "TaskCardChanged",
            Self::WorkspaceChanged { .. } => "WorkspaceChanged",
            Self::DeskTaskCompleted { .. } => "DeskTaskCompleted",
            Self::EmergencyPauseChanged { .. } => "EmergencyPauseChanged",
            Self::TaskDiscussionPosted { .. } => "TaskDiscussionPosted",
            Self::TaskDiscussionRedacted { .. } => "TaskDiscussionRedacted",
            Self::WorkflowEnabledChanged { .. } => "WorkflowEnabledChanged",
            Self::WorkflowReportDelivered { .. } => "WorkflowReportDelivered",
            Self::WorkflowChildCallNotOffered { .. } => "WorkflowChildCallNotOffered",
            Self::WorkflowRunFinished { .. } => "WorkflowRunFinished",
            Self::WorkflowRunStarted { .. } => "WorkflowRunStarted",
            Self::WorkflowNodeStarted { .. } => "WorkflowNodeStarted",
            Self::WorkflowNodeFinished { .. } => "WorkflowNodeFinished",
        }
    }

    /// Whether a retention pass may ever discard this entry (issue #275).
    ///
    /// **Exhaustive on purpose — do not add a `_` arm.** A new event variant
    /// must fail this match and make somebody choose, because the safe default
    /// for an unclassified entry is to keep it forever, and a wildcard would
    /// silently pick the other one.
    ///
    /// Only a handful of kinds are prunable, and each is high-volume machine
    /// exhaust that no other entry addresses:
    ///
    /// - the four workflow-run progress kinds — `WorkflowRunStarted`,
    ///   `WorkflowNodeStarted` (issue #382), `WorkflowNodeFinished` and
    ///   `WorkflowRunFinished` — the growth the issue was filed for, left behind
    ///   when #259 deletes a workflow, and
    /// - `McpCallFailed`, a per-attempt tool failure whose durable meaning is
    ///   already carried by the run outcome it belongs to.
    ///
    /// Everything else is permanent. `WebhookReceived` and `ScheduleFired` are
    /// tempting — both are voluminous and machine-generated — but a webhook
    /// body is the only record of what a counterparty actually sent, and a
    /// schedule tick is the only evidence a cron fired at all. Both are
    /// evidence, so both stay. Chat kinds stay for a second, harder reason:
    /// `OperatorMessage`, `AgentReply` and `TaskDiscussionPosted` are addressed
    /// by sequence from thread parents, reactions, and #358's redaction
    /// tombstone, so pruning one dangles a pointer nothing can repair.
    ///
    /// And a third reason, sharper than either: **something still reads it.**
    /// `WorkflowReportDelivered` is folded at boot by
    /// `runtime::delivered_by_unsettled_runs` to learn what a crashed run
    /// already sent. Pruning it re-delivers already-sent reports to real
    /// people. When classifying a new variant, ask not only "is this evidence"
    /// and "does anything point at it", but "does anything read it back".
    pub fn retention_class(&self) -> crate::ports::events::RetentionClass {
        use crate::ports::events::RetentionClass::{Permanent, Prunable};
        match self {
            Self::WorkflowRunStarted { .. }
            | Self::WorkflowRunFinished { .. }
            | Self::WorkflowNodeStarted { .. }
            | Self::WorkflowNodeFinished { .. }
            | Self::McpCallFailed { .. }
            // Issue #327, and the one place this diverges from its sibling
            // `TaskCardChanged` — which is Permanent — so the reasoning is
            // spelled out rather than assumed.
            //
            // It passes all three of the tests the doc comment above sets. It
            // is not evidence: the tree IS the record of what the workspace
            // holds, and this frame carries no body, only "something moved".
            // Nothing points at it: no entry is addressed by its sequence, the
            // way a reaction or a redaction tombstone addresses a chat message.
            // And nothing reads it back: no boot-time fold consults it, unlike
            // `WorkflowReportDelivered`.
            //
            // What it is instead is high-volume machine exhaust — one frame per
            // keystroke-debounced console save, per agent write, per seeded
            // node at first boot — whose entire meaning is "re-read the tree",
            // and which is worthless the moment the console has done so.
            //
            // `TaskCardChanged` stays Permanent because a board card's
            // lifecycle is the company's work history. A note's revision
            // history is not kept here at all: for a published deliverable it
            // lives on the artifact chain (#552), and for an ordinary note it
            // is not kept anywhere, which pruning this does not change.
            // Issue #1015, put through the three questions above rather than
            // swept in beside its neighbours.
            //
            // Is it evidence? No — the `RunRecord` is the record of an attempt's
            // status, and this frame carries no state the row does not already
            // hold; it says "the row moved". Does anything point at it? No: it
            // is joined by `run_id`, which pruning does not disturb, and nothing
            // is addressed by its sequence. Does anything read it back? No boot
            // fold consults it — `reap_orphaned_runs` reads the *rows*, by
            // status, which is why it can settle runs this frame never covered.
            //
            // And it is high-volume machine exhaust by construction: several
            // frames per attempt, one per transition, on every card and every
            // chat turn. Its entire meaning is "re-read this run", and it is
            // worthless once the console has.
            | Self::RunStatusChanged { .. }
            | Self::WorkspaceChanged { .. }
            // Issue #983, and classified deliberately rather than swept in with
            // its neighbours — the doc above asks for all three questions.
            //
            // Is it evidence? No: it says a turn was accepted, and what the turn
            // did is on its `AgentReply`, its `TurnFailed`, or its run row — all
            // of which outlive it. Does anything point at it? No: nothing is
            // addressed by its sequence the way a reaction or a redaction
            // tombstone addresses a chat message; the turn is joined by
            // `turn_id`, which pruning does not disturb. Does anything read it
            // back? The boot sweep does — and only for turns *this* host left
            // open, which by construction predate no retention pass, since a
            // pass runs on a live company and the sweep runs before one is.
            //
            // What it is instead is one frame per operator message on a
            // high-traffic desk, whose meaning is entirely spent once the turn
            // settles. `TurnFailed` is Permanent below for the opposite reason:
            // it is the only record that a question was accepted and never
            // answered.
            | Self::TurnStarted { .. } => Prunable,

            Self::OperatorMessage { .. }
            | Self::TurnFailed { .. }
            | Self::WebhookReceived { .. }
            | Self::ScheduleFired { .. }
            | Self::A2aTaskReceived { .. }
            | Self::ApprovalParked { .. }
            | Self::ApprovalResolved { .. }
            | Self::FeedbackFiled { .. }
            | Self::PaymentReceived { .. }
            | Self::LifecycleChanged { .. }
            | Self::AgentReply { .. }
            | Self::ReactionToggled { .. }
            | Self::MemoryFactDeleted { .. }
            | Self::ToolAccessChanged { .. }
            | Self::TaskDispatched { .. }
            | Self::WorkflowCreated { .. }
            | Self::WorkflowUpdated { .. }
            | Self::WorkflowDeleted { .. }
            | Self::TaskSteered { .. }
            | Self::TaskCardChanged { .. }
            | Self::DeskTaskCompleted { .. }
            | Self::EmergencyPauseChanged { .. }
            | Self::TaskDiscussionPosted { .. }
            | Self::TaskDiscussionRedacted { .. }
            // Issue #276: an arming change is an audit record of a decision —
            // by an operator, or by the host's disarm rule. Permanent for the
            // same reason its create/update/delete siblings above are: it is the
            // only answer to "when did this stop firing, and who stopped it".
            | Self::WorkflowEnabledChanged { .. }
            // Issue #529: permanent, and this one is load-bearing rather than
            // conventional. It is the durable ledger of reports that already
            // left the process, written write-behind at dispatch so a re-run
            // after a crash can skip them. Pruning it would not lose evidence —
            // it would re-deliver already-sent reports to real people on the
            // next re-run, which is the exact failure the variant exists to
            // prevent. See its own docs.
            | Self::WorkflowReportDelivered { .. } => Permanent,
            // Issue #617: permanent, and it is the clearest kind of evidence
            // this enum carries — the record that a consequential call ran
            // WITHOUT the operator being asked. Pruning it would delete the only
            // trace that the company acted unapproved, which is precisely the
            // question an approvals audit exists to answer. It fails the "is
            // this evidence" test in the strongest possible direction.
            //
            // Nothing reads it back today, and that does not soften the class:
            // the reader is a person reviewing what happened, not a boot-time
            // fold.
            Self::WorkflowChildCallNotOffered { .. } => Permanent,
        }
    }
}

/// How one workflow node's execution came out (issue #371, third arm #881).
///
/// A closed set of **unit** variants on purpose: it is the entire payload of
/// [`CompanyEvent::WorkflowNodeFinished`] beyond the structural ids, and having
/// no `String` arm is what guarantees, by construction, that a node's own error
/// text cannot reach the journal or the wire through this event. Issue #881
/// added a third reading and kept that invariant — [`Blocked`](Self::Blocked)
/// carries nothing, and what it is blocked *on* travels structurally on
/// [`WorkflowRun::blocked_nodes`](crate::ports::WorkflowRun) instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowNodeStatus {
    /// The node executed and produced output.
    Ok,
    /// The node's executor errored (after any retries were exhausted).
    Error,
    /// The node stopped because it needs a human, not because anything went
    /// wrong (issue #881): a tool call inside its turn was parked for operator
    /// approval, so the node produced no deliverable and its branch does not
    /// continue.
    ///
    /// **Never reported by the engine.** tinyflows knows only success and
    /// failure, and an agent node whose turn parked a gated call returns a
    /// capability error so the branch halts — mechanically the right channel,
    /// since `on_error` defaults to `"stop"` and `retry.max_attempts` to `1`.
    /// The *host* then reclassifies that node's row, exactly as
    /// [`WorkflowRun::cancelled`](crate::ports::WorkflowRun) reclassifies a
    /// stopped run: a blocked node is not a failed one, and rolling it into the
    /// failure count would hide real failures among approvals nobody has
    /// answered yet.
    Blocked,
}

/// A `CompanyEvent` durably appended to the log with its sequence and time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    /// The event's monotonic sequence number.
    pub seq: EventSeq,
    /// The company the event belongs to.
    pub company: CompanyId,
    /// The event payload.
    pub event: CompanyEvent,
    /// Epoch-millis timestamp the event was appended.
    pub at_millis: u64,
}

// ---------------------------------------------------------------------------
// Effects, groups, and dispositions
// ---------------------------------------------------------------------------

/// The supervised-policy taxonomy an effect falls into.
///
/// Not named binding in `ports.md`, but drives `ApprovalGate` evaluation under
/// the supervised policy mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectGroup {
    /// Spending money.
    Spend,
    /// Sending a message to a counterparty.
    Send,
    /// Signing or filing a document.
    Sign,
    /// Publishing externally.
    Publish,
    /// Hiring or contracting.
    Hire,
    /// Touching the company's identity.
    Identity,
    /// Anything not otherwise classified.
    Other,
}

/// The residual bucket. Named here rather than inferred, because for a while it
/// silently meant two things at once — see
/// [`Effect::may_be_granted_standing`].
impl EffectGroup {
    /// Is this the catch-all bucket, i.e. did the classifier find no particular
    /// consequence to name on the operator's card?
    ///
    /// This answers the *labelling* question and nothing else. It used to double
    /// as the standing-grant rule, which is what let `shell` and
    /// `workspace_write` be handed over for a week: neither name contains a
    /// consequence word, so both landed here, so both were grantable. That rule
    /// now lives on [`Effect::may_be_granted_standing`], where it is decided by
    /// what the tool can reach.
    pub fn is_unclassified(&self) -> bool {
        matches!(self, Self::Other)
    }
}

/// A side effect the brain wants to perform, submitted to the approval gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    /// The dotted effect kind, e.g. `payment.send`.
    pub kind: String,
    /// The supervised taxonomy group.
    pub group: EffectGroup,
    /// The USD amount involved, if any.
    pub amount_usd: Option<f64>,
    /// Whether this effect continues an established thread.
    pub established_thread: bool,
    /// Whether the counterparty is being contacted for the first time.
    pub first_time_counterparty: bool,
    /// Effect-specific payload.
    pub payload: serde_json::Value,
    /// The roster agent whose **harness tool call** this effect was projected
    /// from, when it was projected from one at all (issue #243).
    ///
    /// This is the discriminator between the two kinds of effect that reach the
    /// same approval queue, and it exists because they need opposite treatment
    /// on approval:
    ///
    /// * `None` — a *native* effect the runtime itself performs
    ///   (`CycleHostImpl::send_email`, the workflow delivery path, a Medulla
    ///   effect frame). Approving it means the runtime executes it, exactly as
    ///   before this field existed.
    /// * `Some(agent_id)` — an effect projected from a tool call openhuman
    ///   already **blocked** inside an agent turn
    ///   ([`ApprovalPolicy::effect_for`](crate::harness::policy::ApprovalPolicy::effect_for)).
    ///   There is nothing for the runtime to execute: the real work is the tool,
    ///   which only that agent can run. Approving it mints a single-use grant and
    ///   re-dispatches the agent instead.
    ///
    /// Only `effect_for` ever stamps this, so `agent.is_some()` is exactly
    /// "came from a harness tool call". Skipped when serializing and defaulted
    /// when absent, so journal lines written before this field existed replay as
    /// `None` — no grant, and the legacy re-ask behaviour — rather than failing
    /// to parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The task **attempt** ([`RunRecord`](crate::ports::runs::RunRecord)) whose
    /// turn produced this effect, when it was produced inside one at all (issue
    /// #242).
    ///
    /// This is the correlation an approval needs to be answerable *about a
    /// run*: the approvals queue is company-wide, so without it "which attempt
    /// is waiting on me?" cannot be asked, and an attempt cannot tell whether it
    /// parked anything of its own.
    ///
    /// Stamped at the **dispatch** boundary, not in
    /// [`ApprovalPolicy::effect_for`](crate::harness::policy::ApprovalPolicy::effect_for):
    /// the policy is per-agent and outlives any one run, so it has no run
    /// context to stamp. An effect a *chat* turn parked therefore stays `None`,
    /// correctly — no attempt is waiting on it.
    ///
    /// Skipped when serializing and defaulted when absent, exactly like
    /// [`agent`](Self::agent), so journal lines written before this field
    /// existed replay as `None` rather than failing to parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl Effect {
    /// The dotted effect kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The supervised taxonomy group.
    pub fn group(&self) -> EffectGroup {
        self.group
    }

    /// The USD amount involved, if any.
    pub fn amount_usd(&self) -> Option<f64> {
        self.amount_usd
    }

    /// Whether this effect continues an established thread.
    pub fn is_established_thread(&self) -> bool {
        self.established_thread
    }

    /// Whether the counterparty is new.
    pub fn is_first_time_counterparty(&self) -> bool {
        self.first_time_counterparty
    }

    /// May an operator turn this parked call into a **standing** permission —
    /// one grant covering repeat calls until a deadline (issues #374, #444)?
    ///
    /// **This is the rule; it lives here so there is one of it.** Three places
    /// enforce it — the mint path, the approval summary the card reads, and the
    /// resolve route's 400 — and all three are in the default build, while the
    /// tool policy that produced the effect compiles only under the `openhuman`
    /// feature. So the rule cannot live in the policy, and expressing it twice
    /// across that seam is the drift issue #444 is about.
    ///
    /// ## Why it is no longer `group == Other`
    ///
    /// `Other` is the bucket a tool falls into when the classifier finds no
    /// consequence word in its name. Reading that as "safe to hand over for a
    /// week" put the three broadest capabilities in the system on the grantable
    /// side — running an arbitrary command, reaching an arbitrary address, and
    /// overwriting the guidance the operator wrote — while a repository read
    /// scoped to one connected account stayed off it. Nothing was malfunctioning;
    /// the rule was measuring a name's vocabulary rather than what the tool can
    /// do.
    ///
    /// It now asks [`consequence_of`](crate::policy::consequence_of), the same
    /// declaration the parking side reads, using the two things an effect
    /// already carries: [`kind`](Self::kind) is the tool name and
    /// [`payload`](Self::payload) is the arguments it was called with. No new
    /// field, so no journal line changes shape and no replayed effect answers
    /// this differently from a live one.
    ///
    /// Arguments matter here, not just the tool: `composio_execute` carries
    /// every Composio action under one name, so the same tool is grantable when
    /// it is listing a repository's pull requests and per-call when it is
    /// sending mail.
    ///
    /// ## A workflow gate is asked about the call it is stopping (issue #1098)
    ///
    /// A gate's `kind` is the wrapper `workflow.approve`, so asking about it
    /// classifies a name the declaration table has never heard of and returns
    /// the undeclared fallback — the classifier never sees the `web_fetch` on
    /// the card. That is a second, independent reason a workflow card is not
    /// grantable today, on top of its `agent: None`, and fixing only the
    /// principal would leave this one refusing every gate.
    ///
    /// [`gate_inner_call`](crate::runtime::workflow_resume::gate_inner_call)
    /// reads the tool and arguments issue #846 already writes onto the payload,
    /// so what is classified is what the card showed. Every other effect takes
    /// the branch below unchanged, which is what keeps the agent path answering
    /// exactly as it did.
    pub fn may_be_granted_standing(&self) -> bool {
        let (kind, payload) = match crate::runtime::workflow_resume::gate_inner_call(self) {
            Some((tool, args)) => (tool, args),
            None => (self.kind.as_str(), &self.payload),
        };
        crate::policy::consequence_of(kind, payload)
            .standing
            .is_grantable()
    }
}

/// How an emitted effect was dispatched by the gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EffectDisposition {
    /// The effect was executed immediately.
    Executed,
    /// The effect was parked and awaits operator approval.
    PendingApproval(ApprovalId),
    /// The effect was denied by policy.
    Denied {
        /// Why the effect was denied.
        reason: String,
    },
}

/// The gate's verdict on an effect, minted without an id.
///
/// Matches the bare `evaluate` return in `ports.md`; the `ApprovalId` for a
/// `RequireApproval` decision is minted separately by `park`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyDecision {
    /// Execute the effect now.
    Allow,
    /// Park the effect for operator approval.
    RequireApproval,
    /// Reject the effect outright.
    Deny,
}

// ---------------------------------------------------------------------------
// Cycle payloads
// ---------------------------------------------------------------------------

/// A compressed summary of one completed cycle, carried as history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompressedTrace {
    /// The cycle this trace summarizes.
    pub cycle_id: String,
    /// A short natural-language summary.
    pub summary: String,
    /// Epoch-millis timestamp the trace was produced.
    pub at_millis: u64,
}

impl CompressedTrace {
    /// Builds a trace stamped with the current time.
    pub fn now(cycle_id: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            cycle_id: cycle_id.into(),
            summary: summary.into(),
            at_millis: now_millis(),
        }
    }
}

/// Metadata for a context chunk, returned by `ContextStore::list`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkMeta {
    /// The chunk's content address.
    pub addr: ChunkAddr,
    /// The chunk's logical label/prefix key.
    pub label: String,
    /// The chunk's length in bytes.
    pub len: usize,
    /// Epoch-millis when the chunk was stored (`0` for rows written before
    /// backends started stamping, so old data reads as "unknown" rather than
    /// as the epoch).
    ///
    /// This is what lets the Brain's "Last updated" stat move when agents write
    /// memory — they write only through this port, never to the `FactStore`
    /// (see `server::ops::memory::memory_stats`).
    ///
    /// Chunks are append-only and never rewritten, so this only ever moves for
    /// a *new* chunk. Backends differ on a re-`put` of an identical body —
    /// sqlite/mongo dedupe on the content address and keep the first write,
    /// the fs index appends a second line — so read freshness as the max
    /// across chunks rather than assuming one row per body.
    #[serde(default)]
    pub stored_at_millis: u64,
}

/// A context chunk's metadata paired with its body, returned by
/// [`ContextStore::list_with_bodies`](crate::ports::ContextStore::list_with_bodies).
///
/// Lets a caller that needs every body under a prefix — the console's Brain
/// list is the one such caller — get them in a single enumeration instead of a
/// `list` followed by one `peek` per row, which on the provider overlay was a
/// full account read each.
#[derive(Clone, Debug, PartialEq)]
pub struct ChunkWithBody {
    /// The chunk's metadata (address, label, length, store time).
    pub meta: ChunkMeta,
    /// The chunk's full body.
    pub body: String,
}

/// A single ledger movement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Epoch-millis timestamp of the entry.
    pub at_millis: u64,
    /// The dotted entry kind, e.g. `inference.spend`.
    pub kind: String,
    /// The signed USD amount.
    pub amount_usd: f64,
    /// A human-readable memo.
    pub memo: String,
}

/// Token **and cost** accounting for a cycle — what the runtime meters onto the
/// Usage surface after the brain returns.
///
/// The cost fields carry no `Eq` (they are `f64`), so this type is `PartialEq`
/// only. Both are `#[serde(default)]` so a peer that predates them still
/// decodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens consumed.
    pub input: u64,
    /// Output tokens produced.
    pub output: u64,
    /// Input tokens served from the provider's KV cache.
    #[serde(default)]
    pub cached_input: u64,
    /// Best-available USD cost for the cycle. Zero when the path reports tokens
    /// but bills elsewhere (the managed `/openai/v1` passthrough echoes no USD).
    #[serde(default)]
    pub cost_usd: f64,
}

impl TokenUsage {
    /// Whether the cycle moved no tokens and cost nothing — the guard that keeps
    /// an idle or offline cycle from writing a meaningless usage sample.
    ///
    /// Includes [`Self::cached_input`]: a cache-served pass is real usage even
    /// if a provider ever reported it without fresh input tokens.
    pub fn is_zero(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cached_input == 0 && self.cost_usd == 0.0
    }

    /// Folds another total into this one — how a brain accumulates the usage of
    /// several passes into one cycle total. Token counts saturate rather than
    /// wrap so a bogus peer value can never underflow the meter.
    pub fn fold(&mut self, other: &TokenUsage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cached_input = self.cached_input.saturating_add(other.cached_input);
        self.cost_usd += other.cost_usd;
    }
}

/// Everything the brain needs to run one cycle.
///
/// **A cycle carries no working memory.** The struct once also carried
/// `compressed_history` (32 recent [`CompressedTrace`]s) and `context_index`
/// (every [`ChunkMeta`] in the company, unbounded), loaded from the
/// [`MemoryStore`](crate::ports::MemoryStore) and
/// [`ContextStore`](crate::ports::ContextStore) on every cycle — and read by no
/// [`Brain`](crate::ports::Brain) implementation. Two facts made them dead
/// rather than merely unused: no summariser exists anywhere in the crate, so a
/// trace's `summary` is a constant string like `harness cycle handled 3
/// event(s)`, and no brain ever consulted either field. A `roster` field was
/// dead on the same terms; every brain re-derives the roster from the company
/// record. Issue #1175 removed all three, so the cycle stops paying an
/// unbounded full scan per turn for a `Vec` it drops.
///
/// The one live recall path is elsewhere and is untouched by this: before each
/// turn `HarnessPool::run` retrieves the top-5 prior task outcomes from the
/// `ContextStore` and injects them as text (`src/harness/memory_loop.rs`, under
/// the `openhuman` feature). Traces are still *written* every cycle
/// (they travel with the export bundle); nothing reads them back. Do not
/// re-add a field here until something consumes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CycleRequest {
    /// Unique id for this cycle.
    pub cycle_id: String,
    /// The company running the cycle.
    pub company_id: CompanyId,
    /// The batch of events driving this cycle.
    pub events: Vec<CompanyEvent>,
    /// The [`EventLog`](crate::ports::EventLog) sequence of each event in
    /// [`Self::events`], positionally aligned. Empty when a caller builds a
    /// request without threading seqs (a brain then falls back to the event's
    /// index); the runtime always populates it so hosted cognition can key its
    /// idempotent `POST /events` on the durable log seq.
    #[serde(default)]
    pub event_seqs: Vec<EventSeq>,
}

/// The brain's output from one cycle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CycleResult {
    /// Messages to emit on channels.
    pub channel_responses: Vec<OutboundMessage>,
    /// New compressed traces to persist.
    pub new_traces: Vec<CompressedTrace>,
    /// Ledger movements produced this cycle.
    pub ledger_deltas: Vec<LedgerEntry>,
    /// Token accounting for the cycle.
    pub token_usage: TokenUsage,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// A brain request to invoke a tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The tool name.
    pub tool: String,
    /// The tool arguments.
    pub args: serde_json::Value,
}

/// The result of a tool invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Whether the tool succeeded.
    pub ok: bool,
    /// The tool's structured output.
    pub output: serde_json::Value,
}

/// A tool advertised in a company's catalog.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool name.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// JSON schema for the tool's arguments.
    pub input_schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Context store
// ---------------------------------------------------------------------------

/// A chunk of context to store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextChunk {
    /// A logical label/prefix key for the chunk.
    pub label: String,
    /// The chunk body.
    pub body: String,
}

/// A search hit from `ContextStore::search`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkHit {
    /// The matching chunk's address.
    pub addr: ChunkAddr,
    /// A snippet of the match.
    pub snippet: String,
    /// A relevance score in `[0, 1]`.
    pub score: f64,
}

/// A context operation the brain issues through `CycleHost::context_op`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextOp {
    /// Store a chunk, returning its address.
    Put(ContextChunk),
    /// List chunks under a prefix.
    List {
        /// The prefix to list under.
        prefix: String,
    },
    /// Read a chunk (optionally a byte range) as text.
    Peek {
        /// The chunk to read.
        addr: ChunkAddr,
        /// An optional byte range within the chunk.
        range: Option<Range<usize>>,
    },
    /// Search chunks for a query.
    Search {
        /// The search query.
        query: String,
        /// The maximum number of hits to return.
        limit: usize,
    },
}

/// The result of a `ContextOp`, one variant per operation return type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextOpResult {
    /// A `Put` returned this address.
    Addr(ChunkAddr),
    /// A `List` returned this metadata.
    Metas(Vec<ChunkMeta>),
    /// A `Peek` returned this text.
    Text(String),
    /// A `Search` returned these hits.
    Hits(Vec<ChunkHit>),
}

// ---------------------------------------------------------------------------
// Memory store
// ---------------------------------------------------------------------------

/// The result of a completed background task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    /// The task id.
    pub task_id: String,
    /// Whether the task succeeded.
    pub ok: bool,
    /// The task output.
    pub output: serde_json::Value,
}

/// A policy for evicting stale memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EvictionPolicy {
    /// Keep only the most recent `n` traces.
    KeepRecent {
        /// How many traces to retain.
        n: usize,
    },
    /// Evict traces older than the given epoch-millis.
    OlderThan {
        /// The cutoff timestamp in epoch millis.
        before_millis: u64,
    },
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// A message arriving on a channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// The channel the message arrived on.
    pub channel: String,
    /// The message text.
    pub text: String,
    /// Who sent it.
    pub from: Actor,
}

/// Channel-specific reply addressing for an [`OutboundMessage`].
///
/// Carries the chat/thread a reply must be delivered back to on channels whose
/// messages are addressed to a specific conversation — chiefly Telegram, where
/// the reply has to land in the same `chat.id` the inbound update came from.
/// The operator channel is a single implicit surface and needs none of this.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyTo {
    /// The chat/thread id to deliver back to. Rendered as a string so it stays
    /// channel-agnostic (Telegram's numeric `chat.id`, a future channel's
    /// opaque thread key) without widening the type per channel.
    pub chat_id: String,
}

/// A message the company emits on a channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutboundMessage {
    /// The channel to emit on.
    ///
    /// A **destination**, never an author. Issue #885: all three
    /// [`AgentReply`](CompanyEvent::AgentReply) writers used to copy this into
    /// `agent_id`, whose contract is "the agent that produced the reply" — so
    /// every bubble emitted on the operator channel was journaled as though the
    /// operator had written it, permanently. Use [`agent`](Self::agent) for who
    /// spoke; this only says where it goes.
    pub channel: String,
    /// The roster teammate that produced this message (issue #885).
    ///
    /// `None` for a message no agent authored — a system notice, a scheduler
    /// tick, a channel-level ack — and for every producer that does not know
    /// its responder. A writer journaling an `AgentReply` falls back to
    /// [`channel`](Self::channel) when this is absent, which is exactly the
    /// pre-#885 behaviour, so nothing regresses on a path that has not been
    /// taught to fill it in.
    ///
    /// Additive on the wire: omitted when absent, so the POST `/chat` body and
    /// every stored record round-trip byte-identically for a producer that
    /// sets nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The message text.
    pub text: String,
    /// The visible processing steps behind this bubble — the agent's tool calls,
    /// thinking runs, and any surfaced MCP failures — folded and scrubbed from
    /// the turn's progress stream (see [`crate::harness::steps`] under the
    /// `openhuman` feature). Per-bubble ownership: the operator bubble carries the
    /// orchestrator's steps; a delegated desk bubble carries that desk lead's
    /// steps.
    ///
    /// Additive and non-secret: the field is omitted on the wire when empty, so
    /// every prior producer (and every non-harness brain, which emits none)
    /// round-trips byte-identically. Never carries raw tool arguments, tool
    /// output, or call ids — only the scrubbed [`TurnStep`] shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<TurnStep>,
    /// Where to deliver the reply, for channels addressed to a specific
    /// chat/thread (Telegram). `None` on the operator channel and on every
    /// message emitted before this field existed; `skip_serializing_if` keeps
    /// such a message byte-identical on the wire, so no stored record migrates
    /// (same `by`/`chat`/`McpCallFailed` additive precedent above).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyTo>,
    /// The board card this bubble's turn **opened**, when it opened one (issue
    /// #246).
    ///
    /// A `spawn_task` used to surface nothing at all: the card appeared on the
    /// board and the operator's reply said nothing about it, so there was no
    /// way to tell a turn that opened work from one that only talked about it.
    /// This is the correlation key that lets the console render a "card opened"
    /// chip, and it is the value journaled onto
    /// [`AgentReply::task_id`](CompanyEvent::AgentReply) so the chip survives a
    /// transcript reload rather than existing only on the live response.
    ///
    /// **Only the first card of a multi-spawn turn.** The journal field it
    /// feeds is a single optional id, and widening it to a list would break the
    /// byte-identical round-trip every stored reply depends on. The claim it
    /// makes — "this reply opened that card" — is therefore true but
    /// incomplete, never false; the bubble's [`steps`](Self::steps) timeline
    /// still shows every `spawn_task` call the turn made.
    ///
    /// Additive and non-secret: a card id, omitted on the wire when absent, so
    /// every prior producer round-trips byte-identically (same `steps` /
    /// `reply_to` precedent above).
    ///
    /// The wire name is pinned to `taskId` rather than inherited, because this
    /// struct — unlike almost everything else the console reads — carries no
    /// `rename_all`. The console sees the same card on three surfaces (this
    /// POST response, the SSE `agent_reply` frame, and the `chat/history` DTO),
    /// and the other two are camelCase; letting this one alone be `task_id`
    /// would be a trap for whoever wires the next reader.
    #[serde(default, rename = "taskId", skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The durable id this bubble was journaled under (issue #364) — the
    /// sequence position of its `AgentReply`, which is the same id
    /// `chat/history` returns for it on a later reload.
    ///
    /// The enabler for everything durable that references a message. Until this
    /// existed a freshly-sent bubble had only a browser-minted counter id, so a
    /// thread reply or a reaction made against it named something no other
    /// reader — a reload, a second operator — could resolve.
    ///
    /// **Stamped by the chat route after journaling, not produced by a brain.**
    /// A brain emits an answer; it does not know where the answer will land in
    /// the log, and every non-chat delivery path (a channel send, a workflow
    /// step) journals nothing at all and correctly leaves this `None`.
    ///
    /// Additive and omitted when absent, so an old console ignores it and a new
    /// console reads its absence as "this host predates durable message ids"
    /// and says so rather than offering an action that cannot persist.
    #[serde(default, rename = "messageId", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// One visible step in an agent turn's processing timeline, surfaced in the
/// operator chat.
///
/// The point of the timeline: a failed tool call becomes visible (instead of a
/// vague acknowledgement), and a memory-served answer — which runs **zero**
/// steps — is distinguishable from a tool-backed one. Folded from the harness
/// progress stream by [`crate::harness::steps`] (compiled under the `openhuman`
/// feature); every field is scrubbed there before it reaches this shape.
///
/// The wire form is additive and camelCase (`elapsedMs`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStep {
    /// What kind of step this is (drives the icon in the UI).
    pub kind: TurnStepKind,
    /// How the step ended.
    pub status: TurnStepStatus,
    /// A short, human label (e.g. "Reading messages", "Thinking"). Derived from
    /// the tool's server-computed `display_label`, else its tool name — never
    /// from tool arguments or output.
    pub label: String,
    /// **What the step was doing** — a compact rendering of the call's
    /// arguments, passed through the *same* host-side redactor an approval card
    /// uses ([`crate::runtime::approval_display`], issue #372) and then bounded.
    ///
    /// This is what makes two calls to the same tool tell apart (issue #411):
    /// two workspace reads used to render identically because nothing about
    /// *what* was read reached the operator.
    ///
    /// Before #411 this field doubled as the failure cause. That moved to
    /// [`result`](Self::result), which is where "what came back" belongs;
    /// already-persisted rows keep whatever string they hold and still render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// **What came back** (issue #411).
    ///
    /// On success: a *summary* — the intrinsic tool's own OpenCompany-authored
    /// output (bounded), or for every other tool a structural shape line
    /// (`"12 items"`, `"2.4 kB"`) and never its content, because a remote body
    /// is exactly what must not reach this surface.
    ///
    /// On a failure or a park: the plain-language cause in the failure's own
    /// terms — the classifier's `cause_plain`, or the intrinsic tool's own
    /// message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// The typed reason a step did not succeed (issue #411), so the console
    /// renders a **known state** rather than parsing prose.
    ///
    /// `None` on a success, on a still-`Running` step, and on a step
    /// [`AwaitingApproval`](TurnStepStatus::AwaitingApproval) — a park is not a
    /// failure, and its status already says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TurnStepFailure>,
    /// The result was **cut** before the agent could read all of it (issue
    /// #410).
    ///
    /// Carried as its own typed flag rather than buried in prose, because a
    /// silently truncated result is a distinct, actionable state: the call
    /// succeeded, and the answer is still incomplete. That combination is
    /// invisible in a status word, which is precisely how #410 stayed hidden.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// How long the step took in milliseconds, when known (tool calls report it;
    /// thinking/note steps do not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// `skip_serializing_if` for a `bool` that defaults to `false`, so a step that
/// was not truncated serializes exactly as it did before the field existed and
/// every stored row round-trips byte-identically.
fn is_false(value: &bool) -> bool {
    !*value
}

impl Default for TurnStep {
    /// A blank tool-call step, so the several construction sites that fill only
    /// the fields they know can say `..TurnStep::default()` instead of
    /// restating every optional field. The `kind`/`status` here are
    /// placeholders that every real caller overrides.
    fn default() -> Self {
        Self {
            kind: TurnStepKind::ToolCall,
            status: TurnStepStatus::Ok,
            label: String::new(),
            detail: None,
            result: None,
            failure: None,
            truncated: false,
            elapsed_ms: None,
        }
    }
}

impl TurnStepStatus {
    /// Whether this status means the step **failed**.
    ///
    /// The one place the question is answered, so the console's "N failed"
    /// count, the destructive tone and any host-side tally cannot disagree.
    /// [`AwaitingApproval`](Self::AwaitingApproval) is deliberately **not** a
    /// failure: it is work waiting on a person (issue #411).
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Error)
    }

    /// The `snake_case` word this status serializes as.
    ///
    /// The live turn-stream frame carries its status as a `&'static str` rather
    /// than as this enum (it is a dumb transport that models no harness types),
    /// so it needs the word without a serde round-trip. Deriving it here means
    /// the live frame and the persisted step cannot disagree about what to call
    /// the same state.
    pub fn wire_word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Running => "running",
            Self::AwaitingApproval => "awaiting_approval",
        }
    }
}

/// The kind of a [`TurnStep`], driving its icon in the timeline. Serialized in
/// `snake_case` (`tool_call` / `thinking` / `note`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStepKind {
    /// A tool call (a paired started/completed pair, or an unmatched one).
    ToolCall,
    /// A run of the model's reasoning, coalesced to a single "Thinking" step.
    Thinking,
    /// A standalone note — e.g. a surfaced MCP failure or the cap-omission
    /// marker.
    Note,
}

/// How a [`TurnStep`] ended. Serialized in `snake_case` (`ok` / `error` /
/// `running` / `awaiting_approval`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStepStatus {
    /// Completed successfully (or an informational step).
    Ok,
    /// Failed — rendered in the destructive tone.
    Error,
    /// Started but no completion was observed by the end of the turn.
    Running,
    /// **Gated: waiting on a person** (issue #411).
    ///
    /// The approval policy refused the call inline and parked the projected
    /// effect on the operator's queue. The turn genuinely could not continue —
    /// but nothing *broke*, and counting it as a failure was the single most
    /// misleading thing the timeline did: the one step an operator can act on
    /// rendered as a crash.
    ///
    /// A distinct status rather than an [`Error`](Self::Error) carrying a
    /// reason, because the "N failed" summary and the destructive tone both key
    /// off the status word. A reason field would have left both still lying.
    AwaitingApproval,
}

/// Why a [`TurnStep`] did not succeed, in the failure's own terms (issue #411).
///
/// The console renders a *known state* off this instead of pattern-matching the
/// prose in [`TurnStep::result`] — a classifier keyed on a display string is the
/// anti-pattern this exists to remove. The variants are a projection of
/// OpenHuman's own `ToolFailureClass`, mapped in one exhaustive `match` in
/// [`crate::harness::steps`], so a class added upstream is a compile error here
/// rather than a silent fall-through to "something went wrong".
///
/// Deliberately **narrower** than the upstream class set: it is the vocabulary
/// an operator can act on, not the vocabulary the classifier reasons in. The
/// two connectivity classes (`ServiceUnavailable` / `ModelConnection`) collapse
/// into [`Unavailable`](Self::Unavailable) because "wait, or check the link" is
/// one action; the two refusal classes (`Denied` / `ApprovalExpired`) collapse
/// into [`Declined`](Self::Declined) for the same reason.
///
/// Serialized in `snake_case`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStepFailure {
    /// A person refused the action, or the approval request expired before
    /// anyone answered. Never retried on its own.
    Declined,
    /// The company's own safety settings refused the action outright.
    BlockedByPolicy,
    /// The credentials the call needed are missing, expired, or were rejected —
    /// an unauthorized response. The fix is a reconnect, not a retry.
    Unauthorized,
    /// The host lacks an operating-system permission the call needed.
    MissingPermission,
    /// A program or application the call needed is not available.
    ///
    /// Only ever claimed for a call that could actually invoke an external
    /// program. A path-reading tool that runs in this process has no app to
    /// install, so its "not there" is [`NotFound`](Self::NotFound) — see
    /// [`crate::harness::steps`] (issue #924).
    MissingApp,
    /// The file, folder, or bundled resource the call named does not exist.
    ///
    /// Distinct from [`MissingApp`](Self::MissingApp) because the remedy is
    /// different in kind: a missing path is fixed by naming a different path,
    /// not by installing software. Both arrive as one `ENOENT` from the
    /// operating system, and telling a server operator to "install or open the
    /// app" when a note is simply absent is unactionable (issue #924).
    NotFound,
    /// The call ran past its deadline and was stopped.
    Timeout,
    /// A service the call depends on — an upstream API, or the model provider —
    /// could not be reached.
    Unavailable,
    /// Genuinely unclassified. The honest residue, and the *only* case that may
    /// read as "something went wrong": every state above used to land here.
    Failed,
}

// ---------------------------------------------------------------------------
// Company records
// ---------------------------------------------------------------------------

/// An operator-added teammate that the version-controlled manifest does not
/// know about. Persisted as an overlay on the [`CompanyRecord`] and merged into
/// the roster at read/build time; the `company.toml` is never rewritten.
/// Roster-only in v1 (no harness `Agent` is minted for an overlay teammate).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayAgent {
    /// The teammate's stable id.
    pub id: String,
    /// The teammate's display name.
    pub name: String,
    /// The teammate's role.
    pub role: String,
    /// An optional description of the teammate's mandate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The per-teammate tool grant: a manifest `[[agent]].tools`-style glob list,
    /// **intersected** with the company's `[tools].allow` at roster-build time
    /// (issue #661 / L5). An **empty** list is the standard company-wide grant
    /// (every allowed tool), exactly as an omitted manifest `tools` line is —
    /// never "no tools". The intersection is narrow-only: this can restrict a
    /// teammate below the company grant, never widen it past it.
    ///
    /// `#[serde(default)]` keeps every overlay record written before this field
    /// existed deserializing unchanged (as an empty list → standard grant), and
    /// `skip_serializing_if` keeps the common empty case out of the persisted
    /// JSON so a standard-grant teammate serializes exactly as it did before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

/// An operator-added desk membership that the version-controlled manifest does
/// not know about. Persisted as an overlay on the [`CompanyRecord`] and merged
/// into a desk's effective membership at read/resolve time; the `company.toml`
/// is never rewritten. Only additions are modelled — a manifest-declared desk
/// member is part of the blueprint and cannot be removed through the overlay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayDeskMember {
    /// The desk (group-chat) id this addition targets.
    pub desk_id: String,
    /// The teammate id added to the desk. Resolves to a manifest agent or an
    /// [`OverlayAgent`].
    pub agent_id: String,
}

/// Where a company's manifest was seeded from — the source template's stable
/// identity, recorded once at launch and carried across rebuilds.
///
/// A company launched from a template directory (`serve --company
/// companies/<slug>`) records the directory slug as its stable `source_id`; a
/// company provisioned from a raw manifest body (`POST /api/v1/companies`)
/// carries no provenance (`CompanyRecord::template_provenance` stays `None`) —
/// provenance is never fabricated for a manifest that did not come from a
/// template. The blueprint (`company.toml`) is never rewritten: provenance
/// lives only on the record/overlay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateProvenance {
    /// The template's stable identifier — the source directory slug (or a
    /// canonical id where one exists). Stable across rebuilds and restarts.
    pub source_id: String,
    /// The template's version, when the source exposes one. `None` when the
    /// template is unversioned.
    #[serde(default)]
    pub version: Option<String>,
    /// The source directory path the company was launched from, when recorded.
    /// Optional and informational; `source_id` is the durable stable key.
    #[serde(default)]
    pub path: Option<String>,
}

/// An operator-set explicit ordering (hierarchy) for one desk's effective
/// members. Persisted as an overlay on the [`CompanyRecord`]; the version-
/// controlled manifest is never rewritten. Applied inside
/// [`CompanyRecord::effective_desk_members`] as a whole-set permutation: ids in
/// `ordered` come first in the given order (so an overlay-added member can be
/// promoted above manifest members — a whole-set reorder, which a per-member
/// `rank` could not express), and any effective member not listed keeps today's
/// relative order after them. Stale ids in `ordered` that are no longer members
/// are simply ignored. An empty `ordered` (or an absent entry) reproduces the
/// blueprint order exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayDeskOrder {
    /// The desk (group-chat) id this ordering targets.
    pub desk_id: String,
    /// The operator-set member id order. Listed ids sort first in this order;
    /// unlisted effective members keep their default relative order after.
    pub ordered: Vec<String>,
}

/// An operator-created desk (group chat) that the version-controlled manifest
/// does not declare. Persisted as an overlay on the [`CompanyRecord`] and merged
/// with the manifest's `[[group_chat]]` desks at read/resolve time; the
/// `company.toml` is never rewritten. This is the desk analogue of
/// [`OverlayAgent`] — the manifest stays authoritative and rebuild-preserved,
/// while runtime-created desks live alongside it and survive rebuilds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayDesk {
    /// The desk id — snake_case, unique across the manifest desks and the other
    /// overlay desks. Doubles as the chat thread id.
    pub id: String,
    /// Human-readable desk name.
    pub name: String,
    /// What the desk is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The desk's founding member ids, in order; the first is its lead. Each
    /// must resolve to a roster teammate (manifest agent or [`OverlayAgent`]).
    /// Further members can still be added through the desk-member overlay.
    #[serde(default)]
    pub members: Vec<String>,
}

/// A workflow graph body authored at runtime (the console's create dialog or
/// the orchestrator's `create_workflow` tool) and persisted on the
/// [`CompanyRecord`] rather than written into the company source tree.
///
/// The source tree (`companies/<name>/workflows/<id>.toml`) is the
/// version-controlled seed and, in hosted mode, a **read-only** crate mount —
/// writing a graph there fails with `EROFS` (issue #168). So a created graph
/// lands here instead, next to the enabled id, and every reader unions the two
/// sets (see [`load_workflow_union`](crate::company::load_workflow_union)).
///
/// The stored value is the **rendered TOML**, already validated at create time:
/// readers re-parse it through
/// [`parse_workflow`](crate::company::parse_workflow), so an overlay graph
/// passes exactly the same validation an on-disk seed file does, with no second
/// model shape to drift. Deliberately no `name` field — the name lives inside
/// the TOML, one source of truth.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayWorkflow {
    /// The workflow id — what the seed file would be named (`<id>.toml`).
    pub id: String,
    /// The rendered, already-validated workflow graph TOML.
    pub toml: String,
}

/// Why a workflow's armed state changed (issue #276). Serialized in
/// `snake_case` (`operator` / `disarmed`).
///
/// The distinction is the point of journaling this at all: "an operator paused
/// this" and "the host refused to arm a schedule nobody had reviewed" are
/// different facts, and only the second one explains a workflow that never fired
/// after it was created.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEnabledReason {
    /// An explicit `PUT …/workflows/{wid}/enabled`.
    Operator,
    /// The disarm rule fired: a create or an edit produced a trigger schedule
    /// that had not been armed before, so the host wrote `false` rather than
    /// letting an unreviewed cron go live. Only ever paired with
    /// `enabled: false` — the rule has no arming direction.
    Disarmed,
}

/// An operator-set daily spend cap for one teammate, persisted on the
/// [`CompanyRecord`] so it wins over the manifest's `budget_usd_daily` without
/// rewriting `company.toml` and without a redeploy (issue #343).
///
/// The manifest is a **boot snapshot** baked into the tenant image, so before
/// this the shipped number was the only number. An entry here is the durable
/// override the console writes; [`CompanyRecord::effective_budget`] is the one
/// place the two are reconciled.
///
/// Three states, and keeping them apart is the point:
///
/// - **no entry** — the manifest value applies (the pre-#343 behaviour exactly);
/// - **entry with `Some(x)`** — capped at `x`, including a legitimate `0.0`
///   ("this teammate may not spend");
/// - **entry with `None`** — explicitly **uncapped**, which beats a manifest cap.
///   Without this state, clearing a cap on a manifest-capped teammate would be
///   impossible: dropping the row would fall back to the very cap being cleared.
///
/// "Cleared" and "zero" are therefore different rows, never the same one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetOverride {
    /// The teammate this caps — a manifest `[[agent]]` id or an
    /// [`OverlayAgent`] id.
    pub agent_id: String,
    /// The cap in USD per UTC day, or `None` for "explicitly uncapped".
    #[serde(default)]
    pub budget_usd_daily: Option<f64>,
    /// Who set it. Attribution is part of the acceptance: a cap that can be
    /// raised anonymously is not much of a cap.
    pub set_by: Actor,
    /// When it was set (epoch millis).
    pub at_millis: u64,
}

impl BudgetOverride {
    /// The first `agent_id` appearing more than once in `entries`, if any.
    ///
    /// For validating a set of overrides this process did not write — an
    /// imported bundle, principally. [`CompanyRecord::budget_override`] reads the
    /// *first* match, so a second row for one teammate is not a harmless
    /// duplicate: it makes the applied cap a function of serialization order.
    /// The two rows can differ in cap *and* in attribution, so there is no
    /// answer to pick — one choice over-restricts a teammate, the other hands
    /// back an allowance an admin revoked, and both name someone in the console
    /// who may not have set it. Callers reject rather than guess.
    ///
    /// Linear scan: an override set is one row per capped teammate, so it is
    /// bounded by roster size.
    pub fn duplicate_agent_id(entries: &[BudgetOverride]) -> Option<&str> {
        let mut seen: Vec<&str> = Vec::with_capacity(entries.len());
        for entry in entries {
            if seen.contains(&entry.agent_id.as_str()) {
                return Some(&entry.agent_id);
            }
            seen.push(&entry.agent_id);
        }
        None
    }
}

/// An operator-set override of the company's `[policy]` block, persisted on the
/// [`CompanyRecord`] so a tier change wins over the manifest without rewriting
/// `company.toml` and without a redeploy (issue #562).
///
/// The company-scoped twin of [`BudgetOverride`], and it exists for the same
/// reason: the manifest is a **boot snapshot** baked into the tenant image, so
/// before this the shipped tier was the only tier an operator could ever run —
/// and the tier is what decides whether they drown in approval cards.
///
/// # Why an overlay and not a manifest write
///
/// This is the constraint that shapes the whole feature. A rebuild
/// (`runtime::builder`) re-persists `record.manifest` **from the seed**, merging
/// only `[workflows].enabled` from the overlays; every other field is
/// seed-authoritative, *"and for `[tools]` / `[policy]` that is a security
/// property — a record-wins merge would let a runtime grant outlive the operator
/// revoking it in version control."*
///
/// So writing `[policy].mode` into `record.manifest` would be wiped by the next
/// rebuild **and** would contradict a stated invariant. An overlay is a
/// different thing: it is not a merge into the blueprint, it is a durable,
/// attributed operator decision that survives the rebuild the way
/// [`overlay_budgets`](CompanyRecord::overlay_budgets) does, and that
/// [`CompanyRecord::effective_policy`] resolves *ahead* of the manifest at read
/// time.
///
/// # Version control still wins when it speaks
///
/// The invariant above is about the seed winning a *merge*, and an override that
/// simply outlived every seed edit would reproduce its named harm by another
/// route: an operator tightens `[policy]` in `company.toml`, redeploys, and the
/// looser console override silently wins — a runtime write outliving a seed
/// rollback. #343 makes the opposite trade for spend caps and is right to; a cap
/// is a number, not the gate.
///
/// So there are **two** clearing paths, and they answer different questions:
///
/// - `runtime::builder::carry_policy_override` drops the override when the
///   seed's `[policy]` **changes** — and only then, so a routine redeploy that
///   said nothing does not silently revert the operator.
/// - `DELETE …/policy` is how an operator clears their own override without
///   touching version control.
///
/// Between seed edits the override is durable, which is what makes it usable at
/// all, and every one is **attributed** (`set_by` + `at_millis`) so the console
/// can show who moved the gate and when.
///
/// # Absent fields mean "not overridden"
///
/// Both fields are optional and independent, so an operator can move the tier
/// without touching the always-ask list, or edit the list while leaving the tier
/// where the manifest put it. `None` is never "empty" — an explicitly emptied
/// always-ask list is `Some(vec![])`, which is a real state an operator can
/// choose and which must not collapse into "fall back to the manifest's three
/// defaults".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyOverride {
    /// The autonomy tier to run, or `None` to leave the manifest's in force.
    ///
    /// A `POLICY_MODES` word. Validated at the write route rather than here:
    /// this type is inert data. [`CompanyRecord::effective_policy`] ignores an
    /// unknown stored value and keeps the manifest's tier, so version skew can
    /// never loosen a stricter seed policy.
    #[serde(default)]
    pub mode: Option<String>,
    /// The always-ask effect kinds, or `None` to leave the manifest's in force.
    ///
    /// `Some(vec![])` is an operator deliberately clearing the list, which is
    /// **not** the same as `None`. It is the operator's real lever and it wins
    /// over every tier including `full`, so the two must stay distinguishable.
    #[serde(default)]
    pub always_approve: Option<Vec<String>>,
    /// Who set it. A tier that can be loosened anonymously is not much of a gate.
    pub set_by: Actor,
    /// When it was set (epoch millis).
    pub at_millis: u64,
}

impl PolicyOverride {
    /// Does this override actually change anything?
    ///
    /// An override with both fields `None` carries only attribution, and
    /// resolving it is a no-op. The write route rejects one rather than
    /// persisting a row that says nothing but whose presence the console renders
    /// as "overridden".
    pub fn is_empty(&self) -> bool {
        self.mode.is_none() && self.always_approve.is_none()
    }
}

/// The operator overlays persisted as a single JSON blob by the string-column
/// stores (sqlite + mongodb `overlay_json`). The filesystem store keeps the two
/// collections as typed fields on its own `Meta` instead.
///
/// [`Self::parse`] accepts both the current object form and the legacy bare
/// array (`overlay_json` held only `Vec<OverlayAgent>` before desk overlays
/// existed), so existing rows load without a migration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OverlayBlob {
    /// The operator team overlay.
    #[serde(default)]
    pub agents: Vec<OverlayAgent>,
    /// The operator desk-membership overlay.
    #[serde(default)]
    pub desk_members: Vec<OverlayDeskMember>,
    /// The operator per-desk member-ordering overlay.
    #[serde(default)]
    pub desk_order: Vec<OverlayDeskOrder>,
    /// The operator-created desk overlay. Absent on rows written before desk
    /// creation existed, so `#[serde(default)]` loads them as empty.
    #[serde(default)]
    pub desks: Vec<OverlayDesk>,
    /// The operator-authored workflow graph bodies. Absent on rows written
    /// before runtime workflow authoring persisted through the store, so
    /// `#[serde(default)]` loads them as empty.
    #[serde(default)]
    pub workflows: Vec<OverlayWorkflow>,
    /// The operator-set per-teammate daily spend caps (issue #343). Absent on
    /// rows written before console budget writes existed, so `#[serde(default)]`
    /// loads them as empty — which is exactly "the manifest still decides".
    #[serde(default)]
    pub budgets: Vec<BudgetOverride>,
    /// The operator's `[policy]` override (issue #562). Absent on rows written
    /// before console policy writes existed, and `#[serde(default)]` reads that
    /// absence as `None` — "the manifest's `[policy]` still decides", which is
    /// the pre-#562 behaviour exactly.
    #[serde(default)]
    pub policy: Option<PolicyOverride>,
    /// The operator-set per-desk tool ceilings. Absent on rows written before
    /// desks could scope tools, and `#[serde(default)]` reads that absence as
    /// "no desk overrides a ceiling" — which leaves the manifest in charge,
    /// exactly as those companies ran.
    #[serde(default)]
    pub desk_tools: std::collections::BTreeMap<String, Vec<String>>,
    /// The workflow ids the operator has switched off (issue #276). Absent on
    /// rows written before the pause switch existed, and `#[serde(default)]`
    /// reads that absence as "nothing is paused" — the pre-#276 behaviour
    /// exactly.
    #[serde(default)]
    pub disabled_workflows: Vec<String>,
    /// The source-template provenance recorded at launch. `None` for companies
    /// provisioned from a raw manifest and for legacy rows written before
    /// provenance existed (the `#[serde(default)]` keeps those rows loading).
    #[serde(default)]
    pub provenance: Option<TemplateProvenance>,
    /// The three answers first-run setup was given, when the company came from
    /// that flow. `None` for every other company and for rows written before it
    /// existed, which `#[serde(default)]` keeps loading.
    ///
    /// Carried in the blob rather than a column for the same reason
    /// [`provenance`](Self::provenance) is: the SQLite and MongoDB backends
    /// rebuild a record field by field, so anything not in here is silently
    /// dropped on the way back out. That would lose the answers Phase 2 builds
    /// workflows from — on exactly the backends a hosted tenant runs, and only
    /// there, which is the worst shape a data-loss bug can take.
    #[serde(default)]
    pub setup: Option<crate::company::setup::SetupAnswers>,
}

impl OverlayBlob {
    /// Builds a blob from a record's overlay collections and provenance.
    pub fn from_record(record: &CompanyRecord) -> Self {
        Self {
            agents: record.overlay_agents.clone(),
            desk_members: record.overlay_desk_members.clone(),
            desk_order: record.overlay_desk_order.clone(),
            desks: record.overlay_desks.clone(),
            workflows: record.overlay_workflows.clone(),
            budgets: record.overlay_budgets.clone(),
            policy: record.overlay_policy.clone(),
            desk_tools: record.overlay_desk_tools.clone(),
            disabled_workflows: record.disabled_workflows.clone(),
            provenance: record.template_provenance.clone(),
            setup: record.setup.clone(),
        }
    }

    /// Parses the persisted blob, accepting both the current object form
    /// (`{"agents":[…],"desk_members":[…],"desks":[…]}`) and the legacy
    /// bare-array form (`[…]`, the pre-desk-overlay value that held only agents).
    /// When the current form parse fails, falls back to the legacy array; if
    /// that also fails the *original* error (from the current-form parse) is
    /// propagated so the caller sees why the object form failed rather than a
    /// misleading "expected sequence" message. New optional keys (`desks`) are
    /// absorbed by `#[serde(default)]`, so no migration is needed.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        match serde_json::from_str::<OverlayBlob>(json) {
            Ok(blob) => Ok(blob),
            Err(original) => serde_json::from_str::<Vec<OverlayAgent>>(json)
                .map(|agents| Self {
                    agents,
                    desk_members: Vec::new(),
                    desk_order: Vec::new(),
                    desks: Vec::new(),
                    workflows: Vec::new(),
                    budgets: Vec::new(),
                    policy: None,
                    desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    provenance: None,
                    // A legacy bare-array row predates first-run setup by a long
                    // way; it can carry no answers.
                    setup: None,
                })
                .map_err(|_| original),
        }
    }
}

/// Ids [`CompanyRecord::mint_agent_id`] will never hand to a teammate, however
/// free the roster leaves them: the always-present operator channel, the two
/// workspace system roots, and the author the runtime speaks under.
///
/// Held as references to the real constants rather than re-typed literals, so a
/// rename of any of the four moves this list with it instead of quietly
/// unreserving a name. Compared case-insensitively, which is why `Agents` and
/// `Desks` cover a minted (always-lowercase) `agents` / `desks`.
///
/// [`SYSTEM_AUTHOR`](crate::ports::SYSTEM_AUTHOR) earns its place for the same
/// reason `OPERATOR_CHANNEL` does, and issue #966 is why it was noticed: it
/// reaches the console's centred system pill **by value**, so a teammate minted
/// onto it would render as the host. `"system"` is an ordinary legal slug —
/// `agent_slug("System")` produces it — which is what separates it from
/// [`CONFINED_AGENT_ID`](crate::ports::CONFINED_AGENT_ID), unmintable by
/// construction because slugs never emit a hyphen.
pub const RESERVED_AGENT_IDS: [&str; 4] = [
    crate::runtime::OPERATOR_CHANNEL,
    crate::company::workspace_scaffold::AGENTS_ROOT,
    crate::company::workspace_scaffold::DESKS_ROOT,
    crate::ports::SYSTEM_AUTHOR,
];

/// A durable company record: charter/roster (manifest) plus ledger and
/// lifecycle state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompanyRecord {
    /// The company id.
    pub id: CompanyId,
    /// The materialized manifest (charter + roster).
    pub manifest: CompanyManifest,
    /// The append-only ledger.
    pub ledger: Vec<LedgerEntry>,
    /// Lifecycle state, e.g. `running`, `paused`, `archived`.
    pub lifecycle: String,
    /// Operator-added teammates not present in the manifest (the team overlay).
    #[serde(default)]
    pub overlay_agents: Vec<OverlayAgent>,
    /// Operator-added desk memberships not present in the manifest (the desk
    /// overlay). Merged into a desk's effective membership at read time.
    #[serde(default)]
    pub overlay_desk_members: Vec<OverlayDeskMember>,
    /// Operator-set per-desk member orderings (the desk-hierarchy overlay).
    /// Applied as a whole-set permutation inside [`Self::effective_desk_members`];
    /// empty = the blueprint order. The manifest is never rewritten.
    #[serde(default)]
    pub overlay_desk_order: Vec<OverlayDeskOrder>,
    /// Operator-created desks not present in the manifest (the desk-creation
    /// overlay). Merged with the manifest's `[[group_chat]]` desks at read time.
    #[serde(default)]
    pub overlay_desks: Vec<OverlayDesk>,
    /// Workflow graphs authored at runtime (console create dialog / orchestrator
    /// `create_workflow`), persisted here instead of in the company source tree —
    /// which is read-only in hosted mode (issue #168). Unioned with the seed
    /// `workflows/*.toml` files by every reader; the seed wins on an id
    /// collision. The `#[serde(default)]` keeps records written before runtime
    /// authoring persisted through the store loading without a migration.
    #[serde(default)]
    pub overlay_workflows: Vec<OverlayWorkflow>,
    /// Operator-set per-teammate daily spend caps that win over the manifest's
    /// `budget_usd_daily` (issue #343). Read through
    /// [`Self::effective_budget`] — never directly — so the console write path,
    /// the roster build and both read surfaces cannot drift. Empty means the
    /// manifest decides, which is byte-for-byte the pre-#343 behaviour; the
    /// `#[serde(default)]` keeps records written before console budget writes
    /// existed loading without a migration.
    ///
    /// **At most one entry per `agent_id`.** [`Self::effective_budget`] reads the
    /// first match, so a second entry for the same teammate is not a harmless
    /// duplicate — it is a silently unreachable cap, and which of the two wins
    /// depends on insertion order rather than on what an admin last decided.
    /// Mutate through [`Self::upsert_budget_override`] rather than pushing, and
    /// check untrusted input with [`Self::duplicate_budget_agent_id`].
    #[serde(default)]
    pub overlay_budgets: Vec<BudgetOverride>,
    /// The operator's `[policy]` override, if one is set (issue #562).
    ///
    /// `None` — the manifest's `[policy]` applies, exactly as before this
    /// existed. Read through [`Self::effective_policy`], never directly, so the
    /// approval gate and the console cannot disagree about which tier is live.
    #[serde(default)]
    pub overlay_policy: Option<PolicyOverride>,
    /// Per-desk tool ceilings the operator has set from the console, keyed on
    /// desk id — the runtime override of a desk's manifest
    /// [`tools`](crate::company::GroupChat::tools).
    ///
    /// Read through [`Self::effective_desk_tools`], never directly, so the
    /// console card and the harness gate cannot disagree about what a desk
    /// permits. An absent key means the manifest decides, which is exactly the
    /// behaviour of every record written before this field existed — the
    /// `#[serde(default)]` is a no-op migration.
    ///
    /// A `BTreeMap` rather than the `Vec<…Override>` shape its neighbours use,
    /// because the key here is genuinely unique: a desk has one ceiling, and the
    /// map makes the "at most one entry" invariant those neighbours have to
    /// document and police a property of the type instead.
    ///
    /// An entry present but **empty** is meaningful and distinct from an absent
    /// one: it is the operator clearing a manifest ceiling back to "this desk
    /// narrows nothing". Removing the key restores the manifest's value; storing
    /// an empty list overrides it.
    #[serde(default)]
    pub overlay_desk_tools: std::collections::BTreeMap<String, Vec<String>>,
    /// Workflow ids the operator has switched **off** (issue #276). A workflow
    /// named here keeps its graph, stays listed and stays runnable by hand, and
    /// is skipped by
    /// [`WorkflowScheduler::tick`](crate::runtime::WorkflowScheduler) — the pause
    /// switch that used to require deleting the workflow outright.
    ///
    /// Read through [`Self::workflow_enabled`] and mutate through
    /// [`Self::set_workflow_enabled`], never directly, so the scheduler gate, the
    /// two read surfaces and the write path cannot drift.
    ///
    /// **A disable list, not an enable list, and that direction is the whole
    /// design.** Absent means enabled, so every record written before this
    /// existed keeps firing exactly as it did — the `#[serde(default)]` is a
    /// no-op migration rather than a silent mass-disable of every saved schedule.
    ///
    /// **Not `[workflows].enabled`.** The manifest list is a *declaration* —
    /// which workflows this company was provisioned with — and
    /// `merge_enabled_workflows` (`src/runtime/builder.rs`, issue #208) rebuilds
    /// it at boot from seed ids ∪ surviving overlay ids. Expressing "off" as
    /// absence from that list would therefore un-express itself on the next
    /// restart, which for a safety switch is the one failure mode that must not
    /// exist. This field is the runtime override the console writes, the same
    /// split [`Self::effective_budget`] draws between a manifest cap and an
    /// operator's.
    ///
    /// Unlike the edit and delete paths, an id here **may name a seed-backed
    /// workflow**. Pausing does not touch the source tree and can only ever
    /// remove capability, so it cannot let a runtime write outlive a seed
    /// rollback the way a record-wins `[tools]` or `[policy]` merge could.
    #[serde(default)]
    pub disabled_workflows: Vec<String>,
    /// Where this company's manifest was seeded from — the source template's
    /// stable identity, stamped once at launch and carried across rebuilds.
    /// `None` for companies provisioned from a raw manifest body. The
    /// `#[serde(default)]` keeps records written before provenance existed
    /// loading without a migration.
    #[serde(default)]
    pub template_provenance: Option<TemplateProvenance>,
    /// What the operator told first-run setup about their business, stored the
    /// moment they answer (see `docs/spec/runtime/company-setup.md`).
    ///
    /// Kept because **Phase 2 must not ask again.** Phase 1 turns these answers
    /// into a roster; the workflow phase turns the same answers into workflows,
    /// and re-interrogating someone who already described their business would
    /// undo the thing setup exists to buy.
    ///
    /// Written even when the operator abandons the flow before the roster
    /// lands: they told us something true about their company, and it costs
    /// nothing to remember it. It is deliberately **not** the "has setup run?"
    /// flag — that question is answered by whether the roster is empty, so a
    /// record stamped by an abandoned run cannot suppress the offer to try
    /// again (decision D4).
    ///
    /// `None` for every company provisioned before setup existed; the
    /// `#[serde(default)]` keeps those records loading without a migration.
    #[serde(default)]
    pub setup: Option<crate::company::setup::SetupAnswers>,
}

/// What a teammate key an operator or a model typed resolves to on a company's
/// roster (issue #1162).
///
/// The three answers a caller has to tell apart. A key that names nothing is a
/// different fact from a key that names two people: the first is a typo or an
/// invention, the second is a collision the operator created and can only fix
/// by renaming or by using an id. Collapsing them — or silently taking the
/// first match — is the misrouting [`CompanyRecord::overlay_agent_ids_by_name`]
/// exists to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeammateResolution {
    /// Exactly one teammate. Carries the **canonical roster id**, never the key
    /// as typed, so every consumer downstream is working in one namespace.
    Agent(String),
    /// Names no teammate at all.
    Unknown,
    /// Names more than one operator-added teammate, because they share a
    /// display name. Carries every colliding id so a refusal can name them.
    Ambiguous(Vec<String>),
}

impl TeammateResolution {
    /// The canonical id when the key named exactly one teammate.
    ///
    /// For the callers that have nothing useful to say about the other two
    /// answers — a cycle guard has no target to compare, a drain has nothing to
    /// deliver to — and where the caller ahead of them has already refused.
    pub fn agent(self) -> Option<String> {
        match self {
            Self::Agent(id) => Some(id),
            Self::Unknown | Self::Ambiguous(_) => None,
        }
    }
}

impl CompanyRecord {
    /// The effective member ids of a desk: the desk's declared members first
    /// (from the manifest `[[group_chat]]` or, for an operator-created desk, the
    /// [`OverlayDesk`]), then any operator-overlay member additions for that
    /// desk, in order and deduplicated on id — then re-ordered by this desk's
    /// operator-set [`OverlayDeskOrder`] if one exists.
    ///
    /// This is the single source of truth for "who is on a desk", shared by the
    /// REST `list_desks` handler and the harness `desk_lead` resolver so the two
    /// cannot drift. Base ordering: declared order is preserved (manifest or
    /// [`OverlayDesk`]), overlay members are appended in insertion order. Then,
    /// if the operator has set an explicit order for this desk, it is applied as
    /// a whole-set permutation — listed ids come first in the operator's order
    /// (so an overlay member can be promoted to the lead slot), and any effective
    /// member the order does not mention keeps its base relative position after
    /// them. With no order override the base order is returned unchanged, so the
    /// first declared member stays the lead by default.
    pub fn effective_desk_members(&self, desk_id: &str) -> Vec<String> {
        let mut members: Vec<String> = self
            .manifest
            .group_chats
            .iter()
            .find(|c| c.id == desk_id)
            .map(|c| c.members.clone())
            .or_else(|| {
                self.overlay_desks
                    .iter()
                    .find(|d| d.id == desk_id)
                    .map(|d| d.members.clone())
            })
            .unwrap_or_default();
        for add in &self.overlay_desk_members {
            if add.desk_id == desk_id && !members.contains(&add.agent_id) {
                members.push(add.agent_id.clone());
            }
        }
        // Apply the operator-set ordering as a whole-set permutation. Listed ids
        // sort first in the operator's order; unlisted members keep their base
        // relative order after (stable sort). Stale ids no longer members are
        // absent from `members`, so they simply have no effect.
        if let Some(order) = self
            .overlay_desk_order
            .iter()
            .find(|o| o.desk_id == desk_id && !o.ordered.is_empty())
        {
            let mut ranked: Vec<(usize, usize, String)> = members
                .into_iter()
                .enumerate()
                .map(|(base_index, id)| {
                    // Listed ids rank by their position in the operator order;
                    // unlisted ids rank after all listed ids, keyed on their base
                    // index so their relative order is preserved by the sort.
                    let key = match order.ordered.iter().position(|listed| *listed == id) {
                        Some(pos) => pos,
                        None => order.ordered.len() + base_index,
                    };
                    (key, base_index, id)
                })
                .collect();
            ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            members = ranked.into_iter().map(|(_, _, id)| id).collect();
        }
        members
    }

    /// One desk's effective tool ceiling: the operator's override when one is
    /// stored, and the manifest's `[[group_chat]].tools` otherwise.
    ///
    /// Empty means the desk narrows nothing. An operator-created overlay desk
    /// has no manifest row at all, so its ceiling is whatever the override says
    /// and empty until somebody sets one.
    pub fn effective_desk_tools(&self, desk_id: &str) -> Vec<String> {
        if let Some(override_tools) = self.overlay_desk_tools.get(desk_id) {
            return override_tools.clone();
        }
        self.manifest
            .group_chats
            .iter()
            .find(|chat| chat.id == desk_id)
            .map(|chat| chat.tools.clone())
            .unwrap_or_default()
    }

    /// The tool ceilings of every desk `agent_id` sits on, in desk order.
    ///
    /// Built from [`effective_desk_members`](Self::effective_desk_members) and
    /// [`effective_desk_tools`](Self::effective_desk_tools) rather than by
    /// reading the manifest directly, so a teammate added to a desk through the
    /// console is scoped by that desk exactly as a manifest member would be —
    /// otherwise the console could seat someone on a restricted desk and leave
    /// them unrestricted.
    ///
    /// Overlay desks are walked after manifest ones, matching how every other
    /// desk reader on this type orders the two sources.
    pub fn agent_desk_tools(&self, agent_id: &str) -> Vec<Vec<String>> {
        let manifest_desks = self.manifest.group_chats.iter().map(|chat| chat.id.clone());
        let overlay_desks = self.overlay_desks.iter().map(|desk| desk.id.clone());
        let mut seen = std::collections::HashSet::new();
        manifest_desks
            .chain(overlay_desks)
            .filter(|desk_id| seen.insert(desk_id.clone()))
            .filter(|desk_id| {
                self.effective_desk_members(desk_id)
                    .iter()
                    .any(|member| member == agent_id)
            })
            .map(|desk_id| self.effective_desk_tools(&desk_id))
            .collect()
    }

    /// Resolves a desk key (an id, or a case-insensitive name) to its canonical
    /// id, searching the manifest desks first and then the operator-created
    /// overlay desks. Lets the harness route to overlay desks by the same
    /// id-or-name key it already accepts for manifest desks.
    pub fn resolve_desk_id(&self, key: &str) -> Option<String> {
        self.manifest
            .group_chats
            .iter()
            .find(|c| c.id == key || c.name.eq_ignore_ascii_case(key))
            .map(|c| c.id.clone())
            .or_else(|| {
                self.overlay_desks
                    .iter()
                    .find(|d| d.id == key || d.name.eq_ignore_ascii_case(key))
                    .map(|d| d.id.clone())
            })
    }

    /// Whether a desk with `desk_id` exists in either the manifest or the
    /// operator-created overlay desks.
    pub fn desk_exists(&self, desk_id: &str) -> bool {
        self.manifest.group_chats.iter().any(|c| c.id == desk_id)
            || self.overlay_desks.iter().any(|d| d.id == desk_id)
    }

    /// Whether `agent_id` names a roster teammate — a manifest agent or an
    /// operator-overlay teammate. The desk overlay may only add ids that resolve
    /// here.
    pub fn is_roster_agent(&self, agent_id: &str) -> bool {
        self.manifest.agents.iter().any(|a| a.id == agent_id)
            || self.overlay_agents.iter().any(|a| a.id == agent_id)
    }

    /// Mints the roster id for a teammate about to be added under
    /// `display_name`: [`agent_slug`] of the name, suffixed `_2`, `_3`, … until
    /// it collides with nothing this record already routes on (issue #686).
    ///
    /// **The only way a write path should name a new overlay teammate.** Both
    /// minting sites — the console `POST …/team` route and the orchestrator's
    /// `add_agent` tool — call it under the shared per-company write lock, so
    /// the check and the save that follows it cannot interleave.
    ///
    /// # Why a suffix rather than a refusal
    ///
    /// An unsuffixed id colliding with a **manifest** agent would be silently
    /// dropped: `build_roster` skips any overlay id a manifest agent already
    /// claims, so the teammate would persist in the record, never materialise,
    /// and report no error. Refusing instead would take away a capability that
    /// exists today — `POST …/team` accepts duplicate display names — and would
    /// leave the tool's caller with a failed call and no human to recover it.
    ///
    /// # What counts as taken
    ///
    /// Every namespace a key typed into an Assignee field or a chat address can
    /// land in, compared case-insensitively:
    ///
    /// * manifest and overlay teammate ids ([`Self::resolve_roster_agent_id`]);
    /// * desk ids **and desk display names** — [`assignee::resolve`] tries desks
    ///   first and [`Self::resolve_desk_id`] matches a desk name ignoring case,
    ///   so a teammate id equal to a desk's name would be unaddressable. The
    ///   comparison is against the name **as written**, not its slug: nothing
    ///   routes on the slug of a desk name, so a desk called "Content Desk"
    ///   leaves `content_desk` free while one called "Content" does not;
    /// * [`RESERVED_AGENT_IDS`] — the operator channel and the two workspace
    ///   system roots.
    ///
    /// [`assignee::resolve`]: crate::runtime::assignee::resolve
    ///
    /// # The id is minted once and never follows a rename
    ///
    /// `PATCH …/team/{agent_id}` edits a teammate's name, role and description
    /// and deliberately leaves the id alone. A name-keyed id would orphan
    /// everything already filed under the old one — the teammate's
    /// `Agents/<id>/` folder, its `WorkspaceOrigin::Agent` stamps, its budget
    /// override row, its desk memberships, its inbox — which is the same trap
    /// name-keyed DM ids sprang on the console's chat journals (issue #364).
    /// So a slug records what a teammate was called when it was created, and
    /// nothing more; the display name is the thing that stays current.
    ///
    /// # Removing a teammate frees its slug again
    ///
    /// `DELETE …/team/{agent_id}` drops the overlay row (and its budget
    /// override), after which re-adding the same name mints the same bare slug.
    /// The new teammate then **adopts the old one's `Agents/<slug>/` folder**,
    /// and that folder's `Agent` origin stamps re-attribute to it. That is the
    /// intended remedy for a typo'd name — the same human, correcting
    /// themselves, keeping the work — but it does mean remove-plus-re-add is not
    /// a way to give a teammate a clean slate. `Agents/<slug>/` is a folder
    /// named for a seat, not a chain of custody for whoever last sat in it.
    pub fn mint_agent_id(&self, display_name: &str) -> String {
        let stem = agent_slug(display_name);
        if !self.roster_id_taken(&stem) {
            return stem;
        }
        (2usize..)
            .map(|n| format!("{stem}_{n}"))
            .find(|candidate| !self.roster_id_taken(candidate))
            .expect("an unbounded suffix sweep always reaches a free id")
    }

    /// Whether `candidate` already names something this record routes on, so
    /// [`Self::mint_agent_id`] must step past it. See that method for why each
    /// namespace counts.
    fn roster_id_taken(&self, candidate: &str) -> bool {
        if RESERVED_AGENT_IDS
            .iter()
            .any(|reserved| candidate.eq_ignore_ascii_case(reserved))
        {
            return true;
        }
        if self.resolve_roster_agent_id(candidate).is_some() {
            return true;
        }
        self.manifest
            .group_chats
            .iter()
            .map(|c| (&c.id, &c.name))
            .chain(self.overlay_desks.iter().map(|d| (&d.id, &d.name)))
            .any(|(id, name)| {
                id.eq_ignore_ascii_case(candidate) || name.eq_ignore_ascii_case(candidate)
            })
    }

    /// Resolves an operator-typed teammate key to its canonical roster id,
    /// searching manifest agents first and then the overlay teammates.
    ///
    /// The case-insensitive companion to [`Self::is_roster_agent`], for the one
    /// place a human types the key by hand: a card's `assignee` (issue #205).
    /// [`Self::resolve_desk_id`] already accepts a desk by id **or** name
    /// case-insensitively, so an assignee naming a desk resolved while the same
    /// string naming a teammate — `"Engineer"` for `engineer` — did not.
    ///
    /// Matches on **id only** (never `role`, never an overlay teammate's
    /// display name) so the key stays one unambiguous namespace; folding the
    /// case is what stops a typed capital reading as an unknown agent.
    ///
    /// [`Self::is_roster_agent`] keeps its exact-match contract: it guards the
    /// desk overlay, whose ids are machine-written rather than typed.
    pub fn resolve_roster_agent_id(&self, agent_key: &str) -> Option<String> {
        self.manifest
            .agents
            .iter()
            .map(|a| &a.id)
            .chain(self.overlay_agents.iter().map(|a| &a.id))
            .find(|id| id.eq_ignore_ascii_case(agent_key))
            .cloned()
    }

    /// Canonical ids of operator-added teammates whose **display name** matches
    /// `name_key` case-insensitively.
    ///
    /// The companion to [`Self::resolve_roster_agent_id`] for the half of the
    /// roster that has no typable id. `server::ops::team` minted an overlay
    /// teammate with `id: generate_id()` before #686, so an operator who added
    /// "Shane" never saw anything but the name — matching on ids alone made
    /// every teammate they added unassignable, on a board whose Assignee field
    /// is free text with no picker.
    ///
    /// A teammate added since #686 carries a readable slug and resolves by id,
    /// but this stays load-bearing: records written before it keep their
    /// generated ids (nothing migrates them), and
    /// [`Self::mint_agent_id`] deliberately does not re-mint on a rename, so a
    /// renamed teammate's current name is reachable only here.
    ///
    /// Manifest agents keep their id-only namespace: their ids
    /// (`ceo`, `engineer`) are human-authored and typable, and
    /// [`Self::resolve_roster_agent_id`] is tried first, so a display name can
    /// never shadow a real id.
    ///
    /// Returns **every** match rather than the first, so the caller can tell a
    /// unique name from a collision the operator created. Silently routing to
    /// whichever teammate was added first would reintroduce exactly the
    /// misrouting this resolver exists to end.
    pub fn overlay_agent_ids_by_name(&self, name_key: &str) -> Vec<String> {
        self.overlay_agents
            .iter()
            .filter(|a| a.name.eq_ignore_ascii_case(name_key))
            .map(|a| a.id.clone())
            .collect()
    }

    /// Resolves a teammate key the way every surface that takes one should:
    /// **id first, then an operator-added teammate's display name** (issue
    /// #1162).
    ///
    /// The single place the two halves of the roster's namespace are joined.
    /// [`Self::resolve_roster_agent_id`] is deliberately id-only and
    /// [`Self::overlay_agent_ids_by_name`] deliberately name-only; every caller
    /// that wants "who did the human mean" needs both, in this order, and
    /// before #1162 only the board's assignee field had them. `query_company`
    /// printed an overlay teammate under its display name while
    /// `delegate_to_teammate` grounded ids alone, so the orchestrator read a
    /// name off the roster it was told was authoritative and was refused.
    ///
    /// **Ids win.** Trying the id namespace first is what stops a display name
    /// shadowing a real id: a teammate mischievously (or accidentally) named
    /// `"engineer"` can never intercept work meant for the manifest agent
    /// `engineer`. That ordering is a guarantee, not an optimisation — it is
    /// why this is one method rather than a convention each caller re-applies.
    ///
    /// **Manifest agents are not matched by role**, only by id. Two teammates
    /// may legitimately share a role, so role-matching belongs to the surfaces
    /// that can ask a human which one they meant — the workflow authoring
    /// resolver does it deliberately, and stays separate for that reason.
    ///
    /// **Desks are not in scope here.** A caller that accepts a desk *and* a
    /// teammate — [`assignee::resolve`] is the one — must try
    /// [`Self::resolve_desk_id`] itself, first: a desk whose id matches a
    /// teammate id keeps routing as a desk. Folding desks in here would teach
    /// `delegate_to_teammate` to accept them, contradicting its own "that is a
    /// desk, not a teammate" refusal.
    ///
    /// [`assignee::resolve`]: crate::runtime::assignee::resolve
    pub fn resolve_teammate_key(&self, key: &str) -> TeammateResolution {
        let key = key.trim();
        if key.is_empty() {
            return TeammateResolution::Unknown;
        }
        if let Some(id) = self.resolve_roster_agent_id(key) {
            return TeammateResolution::Agent(id);
        }
        let mut by_name = self.overlay_agent_ids_by_name(key);
        match by_name.len() {
            0 => TeammateResolution::Unknown,
            1 => TeammateResolution::Agent(by_name.remove(0)),
            _ => TeammateResolution::Ambiguous(by_name),
        }
    }

    /// This teammate's operator-set budget override, if one exists.
    ///
    /// The presence of a row is itself information — it is what the console
    /// renders the "set by … " attribution line from, and what tells "reset to
    /// the manifest default" (drop the row) apart from "remove the cap" (a row
    /// whose `budget_usd_daily` is `None`). Callers that only want the number
    /// should use [`Self::effective_budget`].
    pub fn budget_override(&self, agent_id: &str) -> Option<&BudgetOverride> {
        self.overlay_budgets
            .iter()
            .find(|entry| entry.agent_id == agent_id)
    }

    /// The daily USD cap actually in force for `agent_id`: the operator's
    /// override when one is stored, else the manifest's `budget_usd_daily`,
    /// else `None` (uncapped).
    ///
    /// **The single source of truth for "what may this teammate spend today"**,
    /// in the shape of [`Self::effective_desk_members`]. The harness gate, the
    /// per-agent [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) arm,
    /// the REST roster and the GraphQL roster all read through here, so a cap
    /// raised in the console cannot be honoured by one and ignored by another.
    ///
    /// An **overlay** teammate has no manifest row at all, so before #343 it was
    /// unconditionally uncapped; now a stored override caps it like any other.
    /// A stored `Some(0.0)` really does mean zero, and a stored `None` really
    /// does mean uncapped even when the manifest names a cap — that asymmetry is
    /// the whole reason the override is `Option<f64>` rather than `f64`.
    pub fn effective_budget(&self, agent_id: &str) -> Option<f64> {
        match self.budget_override(agent_id) {
            Some(entry) => entry.budget_usd_daily,
            None => self
                .manifest
                .agents
                .iter()
                .find(|a| a.id == agent_id)
                .and_then(|a| a.budget_usd_daily),
        }
    }

    /// Stores `entry` as **the** override for its teammate, replacing any entry
    /// already held for that `agent_id`.
    ///
    /// The one way a write path should add to [`Self::overlay_budgets`]. Pushing
    /// directly is what lets a record accumulate two rows for one teammate, and
    /// [`Self::budget_override`] reads the *first* — so the stale row would keep
    /// winning and every surface would agree on a cap no admin last set. Making
    /// the replacement part of the type rather than a convention each caller
    /// remembers is the point: there is no correct way to append.
    pub fn upsert_budget_override(&mut self, entry: BudgetOverride) {
        self.overlay_budgets
            .retain(|held| held.agent_id != entry.agent_id);
        self.overlay_budgets.push(entry);
    }

    /// The `[policy]` actually in force: the operator's override where it sets a
    /// field, the manifest's `[policy]` everywhere else (issue #562).
    ///
    /// **The single source of truth for "which tier is this company running"**,
    /// in the shape of [`Self::effective_budget`]. `build_roster`'s
    /// [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy), the workflow
    /// capability bundle and the console read through here, so a tier changed in
    /// the console cannot be honoured by one and ignored by another.
    ///
    /// Returns an owned [`Policy`] rather than a borrow because the effective
    /// value may not exist anywhere to borrow from — it is a field-wise merge of
    /// two sources.
    ///
    /// The merge is per field and `None` means "not overridden", so an operator
    /// who moved the tier has not thereby silently reset the always-ask list to
    /// the manifest's. An explicitly emptied list (`Some(vec![])`) survives as
    /// empty; only an absent field falls through. An unknown stored mode also
    /// falls through to the manifest: it can arise under version skew, and
    /// allowing the policy parser to downgrade it to `supervised` would loosen
    /// a `readonly` manifest.
    pub fn effective_policy(&self) -> Policy {
        let manifest = &self.manifest.policy;
        let Some(override_) = &self.overlay_policy else {
            return manifest.clone();
        };
        Policy {
            mode: override_
                .mode
                .as_deref()
                .filter(|mode| POLICY_MODES.contains(mode))
                .map(str::to_owned)
                .unwrap_or_else(|| manifest.mode.clone()),
            always_approve: override_
                .always_approve
                .clone()
                .unwrap_or_else(|| manifest.always_approve.clone()),
            // Not overridable from the console today. Left reading the manifest
            // rather than added to `PolicyOverride` speculatively: the issue asks
            // for the tier and the always-ask list, and a spend threshold whose
            // console control does not exist would be a field nothing can write.
            auto_approve_under_usd: manifest.auto_approve_under_usd,
            // Same reasoning for the approval deadline (issue #971): the knob is
            // a manifest one, so the override carries the manifest's answer
            // through unchanged rather than gaining a field no console control
            // writes.
            approval_ttl_hours: manifest.approval_ttl_hours,
        }
    }

    /// The first `agent_id` on this record carrying more than one override, if
    /// any. See [`BudgetOverride::duplicate_agent_id`].
    pub fn duplicate_budget_agent_id(&self) -> Option<&str> {
        BudgetOverride::duplicate_agent_id(&self.overlay_budgets)
    }

    /// Whether `wid` is switched on (issue #276) — the single predicate the
    /// scheduler gate and both read surfaces share.
    ///
    /// Enabled is the default: an id this record has never heard of is on, which
    /// is what makes [`Self::disabled_workflows`] a no-op for every record
    /// written before the switch existed.
    pub fn workflow_enabled(&self, wid: &str) -> bool {
        !self.disabled_workflows.iter().any(|id| id == wid)
    }

    /// Switches `wid` on or off, returning whether the record actually changed.
    ///
    /// Idempotent in both directions, and the `bool` is why: the write path uses
    /// it to skip the store save and the audit event when an operator toggles a
    /// workflow to the state it is already in, so the journal records decisions
    /// rather than clicks.
    ///
    /// Deliberately the **only** mutator. The disarm rules in
    /// `workflow_create.rs` call it with `false` and nothing else ever calls it
    /// with `true` except the explicit enable route — keeping the "an edit
    /// disarms, it never re-arms" invariant in one place instead of in every
    /// caller's discipline.
    pub fn set_workflow_enabled(&mut self, wid: &str, enabled: bool) -> bool {
        let held = self.disabled_workflows.iter().any(|id| id == wid);
        match (enabled, held) {
            // Already in the requested state.
            (true, false) | (false, true) => false,
            (true, true) => {
                self.disabled_workflows.retain(|id| id != wid);
                true
            }
            (false, false) => {
                self.disabled_workflows.push(wid.to_string());
                true
            }
        }
    }
}

/// A compact company listing entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompanySummary {
    /// The company id.
    pub id: CompanyId,
    /// The display name.
    pub name: String,
    /// Lifecycle state.
    pub lifecycle: String,
}

// ---------------------------------------------------------------------------
// Agent economy (tiny.place seam)
// ---------------------------------------------------------------------------

/// A company's tiny.place identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompanyIdentity {
    /// The company id.
    pub company: CompanyId,
    /// The tiny.place `@handle`.
    pub handle: String,
}

/// The registration state of a company on tiny.place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RegistrationState {
    /// Not yet registered.
    Unregistered,
    /// Registered under this address.
    Registered {
        /// The registered agent address.
        addr: AgentAddr,
    },
}

/// A published Agent Card advertising a company's skills on tiny.place.
///
/// The three original fields (`handle`, `description`, `skills`) are unchanged;
/// every field added for the A2A wire shape carries `#[serde(default)]` so
/// records written by earlier phases round-trip without loss.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentCard {
    /// The advertised `@handle`.
    pub handle: String,
    /// A short description of the company.
    pub description: String,
    /// The advertised skill ids.
    pub skills: Vec<String>,
    /// Human-readable display name (the company name).
    #[serde(default)]
    pub name: String,
    /// The actor kind; always `"agent"` for a company.
    #[serde(default)]
    pub actor_type: String,
    /// The A2A endpoint, e.g. `https://host/a2a/{handle}`.
    #[serde(default)]
    pub endpoint: String,
    /// Interfaces the endpoint speaks, e.g. `["a2a-jsonrpc"]`.
    #[serde(default)]
    pub supported_interfaces: Vec<String>,
    /// Capability tokens derived from the advertised skills.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Free-form discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Per-skill payment requirements advertised to counterparties.
    #[serde(default)]
    pub payment_requirements: Vec<CardPayment>,
}

/// A single priced skill on an [`AgentCard`], in x402 terms.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CardPayment {
    /// The skill this price applies to.
    pub skill_id: String,
    /// The decimal price string, e.g. `"25.00"`.
    pub price: String,
    /// The settlement asset, e.g. `"USDC"`.
    pub asset: String,
    /// The settlement network, e.g. `"solana"`.
    pub network: String,
}

/// An addressable agent on tiny.place.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentAddr(pub String);

/// A task sent agent-to-agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct A2aTask {
    /// The requested skill id.
    pub skill: String,
    /// The task input.
    pub input: serde_json::Value,
}

/// A handle to a dispatched A2A task.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct A2aTaskHandle(pub String);

/// A payment requirement quoted by a counterparty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequirement {
    /// The counterparty address.
    pub to: AgentAddr,
    /// The amount due, in USD.
    pub amount_usd: f64,
    /// What the payment is for.
    pub memo: String,
}

/// A firm quote a company can pay against.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    /// A unique quote id.
    pub quote_id: String,
    /// The counterparty address.
    pub to: AgentAddr,
    /// The quoted amount, in USD.
    pub amount_usd: f64,
}

/// The budget envelope a payment must fit within.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetScope {
    /// The remaining budget for this scope, in USD.
    pub remaining_usd: f64,
    /// A label describing the scope (e.g. an agent id).
    pub label: String,
}

/// A receipt for a completed payment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaymentReceipt {
    /// The quote that was paid.
    pub quote_id: String,
    /// The amount paid, in USD.
    pub amount_usd: f64,
    /// Epoch-millis timestamp of the payment.
    pub at_millis: u64,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::workflow_runner::DeliveryStatus;

    /// The answers must survive the **blob**, not merely the record.
    ///
    /// `CompanyRecord` gained a `setup` field and the fs store round-tripped it
    /// for free, because it serialises the whole record. SQLite and MongoDB do
    /// not: they rebuild a record field by field from `OverlayBlob`, so anything
    /// missing there is dropped silently on the way back out — losing exactly the
    /// answers Phase 2 builds workflows from, on exactly the backends a hosted
    /// tenant runs, and nowhere else. `--all-features` compilation is what
    /// surfaced it; this is what keeps it surfaced.
    #[test]
    fn the_setup_answers_survive_the_overlay_blob() {
        let answers = crate::company::setup::SetupAnswers {
            industry: "E-commerce — homeware".into(),
            team_hint: "someone on dispatch".into(),
            automate: "meta ads, order dispatch".into(),
        };
        let mut record = CompanyRecord {
            id: CompanyId::new("acme"),
            manifest: toml::from_str("[company]\nname = \"Acme\"\n").expect("manifest"),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: Some(answers.clone()),
        };

        let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
        let parsed = OverlayBlob::parse(&json).expect("parse");
        assert_eq!(
            parsed.setup,
            Some(answers),
            "the answers were dropped by the blob the SQL backends rebuild from"
        );

        // A company that never went through setup carries nothing, and a row
        // written before the field existed still loads.
        record.setup = None;
        let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
        assert_eq!(OverlayBlob::parse(&json).expect("parse").setup, None);
        assert_eq!(
            OverlayBlob::parse("{\"agents\":[]}")
                .expect("legacy row")
                .setup,
            None
        );
    }

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    // ── Issue #174: cycle usage carries cost, and folds ─────────────────────

    /// A cycle with nothing to report writes nothing, and any single non-zero
    /// field makes it real usage — including a token-less charge.
    #[test]
    fn token_usage_is_zero_only_when_every_field_is() {
        assert!(TokenUsage::default().is_zero());
        for usage in [
            TokenUsage {
                input: 1,
                ..TokenUsage::default()
            },
            TokenUsage {
                output: 1,
                ..TokenUsage::default()
            },
            TokenUsage {
                cached_input: 1,
                ..TokenUsage::default()
            },
            TokenUsage {
                cost_usd: 0.0001,
                ..TokenUsage::default()
            },
        ] {
            assert!(!usage.is_zero(), "{usage:?} is real usage");
        }
    }

    /// Several model passes in one cycle accumulate into one total.
    #[test]
    fn token_usage_folds_passes_together() {
        let mut total = TokenUsage::default();
        total.fold(&TokenUsage {
            input: 100,
            output: 20,
            cached_input: 10,
            cost_usd: 0.01,
        });
        total.fold(&TokenUsage {
            input: 50,
            output: 5,
            cached_input: 0,
            cost_usd: 0.02,
        });
        assert_eq!(total.input, 150);
        assert_eq!(total.output, 25);
        assert_eq!(total.cached_input, 10);
        assert!((total.cost_usd - 0.03).abs() < 1e-9);
    }

    /// A bogus peer value must never wrap the meter into a huge or tiny number.
    #[test]
    fn token_usage_fold_saturates_instead_of_overflowing() {
        let mut total = TokenUsage {
            input: u64::MAX,
            output: u64::MAX,
            cached_input: u64::MAX,
            cost_usd: 0.0,
        };
        total.fold(&TokenUsage {
            input: 10,
            output: 10,
            cached_input: 10,
            cost_usd: 0.0,
        });
        assert_eq!(total.input, u64::MAX);
        assert_eq!(total.output, u64::MAX);
        assert_eq!(total.cached_input, u64::MAX);
    }

    /// The cost fields are additive on the wire: a peer that predates them still
    /// decodes, and an all-zero usage still serializes them for a peer that has
    /// them.
    #[test]
    fn token_usage_decodes_a_payload_without_the_cost_fields() {
        let legacy: TokenUsage = serde_json::from_str(r#"{"input":7,"output":3}"#).unwrap();
        assert_eq!(legacy.input, 7);
        assert_eq!(legacy.output, 3);
        assert_eq!(legacy.cached_input, 0);
        assert_eq!(legacy.cost_usd, 0.0);
        assert_eq!(round_trip(&legacy), legacy);
    }

    /// The `TurnStep` wire shape is camelCase with snake_case enum values:
    /// `{kind, status, label, detail?, elapsedMs?}`. Locks the contract the
    /// console `TurnStep` mirror in `frontend/src/api/types.ts` depends on.
    #[test]
    fn turn_step_wire_shape_is_camel_case_with_snake_case_enums() {
        let step = TurnStep {
            kind: TurnStepKind::ToolCall,
            status: TurnStepStatus::Error,
            label: "Searching the web".to_string(),
            detail: Some("brave · search".to_string()),
            elapsed_ms: Some(1234),
            ..TurnStep::default()
        };
        let json = serde_json::to_value(&step).unwrap();
        assert_eq!(json["kind"], "tool_call");
        assert_eq!(json["status"], "error");
        assert_eq!(json["label"], "Searching the web");
        assert_eq!(json["detail"], "brave · search");
        assert_eq!(json["elapsedMs"], 1234);
        assert_eq!(round_trip(&step), step);
    }

    /// A step with no detail/elapsed omits both keys, and every kind/status
    /// value serializes to its documented snake_case token.
    #[test]
    fn turn_step_omits_absent_fields_and_covers_every_variant() {
        let bare = TurnStep {
            kind: TurnStepKind::Thinking,
            status: TurnStepStatus::Ok,
            label: "Thinking".to_string(),
            detail: None,
            elapsed_ms: None,
            ..TurnStep::default()
        };
        let json = serde_json::to_value(&bare).unwrap();
        assert_eq!(json["kind"], "thinking");
        assert_eq!(json["status"], "ok");
        assert!(json.get("detail").is_none(), "absent detail is omitted");
        assert!(json.get("elapsedMs").is_none(), "absent elapsed is omitted");

        assert_eq!(serde_json::to_value(TurnStepKind::Note).unwrap(), "note");
        assert_eq!(
            serde_json::to_value(TurnStepStatus::Running).unwrap(),
            "running"
        );
    }

    /// `OutboundMessage.steps` is additive: an empty timeline is omitted from
    /// the wire entirely (so every prior producer round-trips byte-identically),
    /// and a legacy `{channel, text}` payload still loads with an empty `steps`.
    #[test]
    fn outbound_message_steps_are_additive_and_omitted_when_empty() {
        let no_steps = OutboundMessage {
            message_id: None,
            task_id: None,
            channel: "operator".to_string(),
            agent: None,
            text: "hi".to_string(),
            steps: Vec::new(),
            reply_to: None,
        };
        let json = serde_json::to_string(&no_steps).unwrap();
        assert_eq!(json, r#"{"channel":"operator","text":"hi"}"#);

        let legacy: OutboundMessage =
            serde_json::from_str(r#"{"channel":"operator","text":"hi"}"#).unwrap();
        assert!(legacy.steps.is_empty());

        let with_steps = OutboundMessage {
            message_id: None,
            task_id: None,
            channel: "operator".to_string(),
            agent: None,
            text: "done".to_string(),
            steps: vec![TurnStep {
                kind: TurnStepKind::Note,
                status: TurnStepStatus::Error,
                label: "MCP: brave unavailable".to_string(),
                detail: Some("server rejected the call".to_string()),
                elapsed_ms: None,
                ..TurnStep::default()
            }],
            reply_to: None,
        };
        assert_eq!(round_trip(&with_steps), with_steps);
    }

    /// Issue #246: `OutboundMessage.task_id` is additive on exactly the same
    /// terms as `steps` above — a bubble that opened no card must serialize
    /// byte-for-byte as it did before the field existed, and a payload written
    /// before it existed must still load. Without both halves every already-
    /// stored response would change shape the moment this field shipped.
    #[test]
    fn outbound_message_task_id_is_additive_and_omitted_when_absent() {
        let no_card = OutboundMessage {
            message_id: None,
            task_id: None,
            channel: "operator".to_string(),
            agent: None,
            text: "hi".to_string(),
            steps: Vec::new(),
            reply_to: None,
        };
        assert_eq!(
            serde_json::to_string(&no_card).unwrap(),
            r#"{"channel":"operator","text":"hi"}"#,
            "a bubble that opened no card keeps the pre-#246 wire form"
        );

        let legacy: OutboundMessage =
            serde_json::from_str(r#"{"channel":"operator","text":"hi"}"#).unwrap();
        assert!(legacy.task_id.is_none());

        let with_card = OutboundMessage {
            message_id: None,
            task_id: Some("t-42".to_string()),
            channel: "operator".to_string(),
            agent: None,
            text: "opened one".to_string(),
            steps: Vec::new(),
            reply_to: None,
        };
        assert_eq!(round_trip(&with_card), with_card);
        assert!(
            serde_json::to_string(&with_card)
                .unwrap()
                .contains(r#""taskId":"t-42""#),
            "the console reads the card off a camelCase key"
        );
    }

    /// `AgentReply.steps` is additive the same way: a reply journaled before
    /// the field existed loads with an empty timeline, and a tool-less reply
    /// omits the key so its on-disk form is byte-identical to the legacy log.
    #[test]
    fn agent_reply_steps_are_additive_and_omitted_when_empty() {
        let legacy: CompanyEvent = serde_json::from_str(
            r#"{"kind":"AgentReply","chat_id":"main","agent_id":"ceo","text":"hi"}"#,
        )
        .expect("a pre-steps AgentReply still loads");
        match &legacy {
            CompanyEvent::AgentReply { steps, .. } => assert!(steps.is_empty()),
            other => panic!("expected AgentReply, got {other:?}"),
        }

        // A tool-less reply serializes without the `steps` key.
        let tool_less = CompanyEvent::AgentReply {
            parent: None,
            task_id: None,
            chat_id: "main".to_string(),
            agent_id: "ceo".to_string(),
            text: "hi".to_string(),
            steps: Vec::new(),
        };
        let json = serde_json::to_value(&tool_less).unwrap();
        assert!(json.get("steps").is_none());

        // A reply with a timeline round-trips it.
        let with_steps = CompanyEvent::AgentReply {
            parent: None,
            task_id: None,
            chat_id: "main".to_string(),
            agent_id: "ceo".to_string(),
            text: "done".to_string(),
            steps: vec![TurnStep {
                kind: TurnStepKind::ToolCall,
                status: TurnStepStatus::Ok,
                label: "Reading messages".to_string(),
                detail: None,
                elapsed_ms: Some(12),
                ..TurnStep::default()
            }],
        };
        let back: CompanyEvent =
            serde_json::from_str(&serde_json::to_string(&with_steps).unwrap()).unwrap();
        assert_eq!(back, with_steps);
    }

    /// #185: the `task_id` correlation key is additive in both directions —
    /// an event journaled before it existed still loads, and an untagged event
    /// still serializes byte-for-byte as it did before the field was added.
    ///
    /// That second half is the migration-free guarantee: every already-persisted
    /// `AgentReply` / `McpCallFailed` in every company's log must round-trip
    /// unchanged, or the cross-backend export/import comparison breaks.
    #[test]
    fn task_id_correlation_is_additive_and_omitted_when_absent() {
        let legacy: CompanyEvent = serde_json::from_str(
            r#"{"kind":"AgentReply","chat_id":"main","agent_id":"ceo","text":"hi"}"#,
        )
        .expect("a pre-task_id AgentReply still loads");
        match &legacy {
            CompanyEvent::AgentReply { task_id, .. } => assert!(task_id.is_none()),
            other => panic!("expected AgentReply, got {other:?}"),
        }

        // An untagged reply keeps the legacy wire shape exactly.
        let untagged = CompanyEvent::AgentReply {
            parent: None,
            chat_id: "main".to_string(),
            agent_id: "ceo".to_string(),
            text: "hi".to_string(),
            steps: Vec::new(),
            task_id: None,
        };
        assert_eq!(
            serde_json::to_string(&untagged).unwrap(),
            r#"{"kind":"AgentReply","chat_id":"main","agent_id":"ceo","text":"hi"}"#
        );

        // A dispatch-produced reply carries the key and round-trips.
        let tagged = CompanyEvent::AgentReply {
            parent: None,
            chat_id: "t-1".to_string(),
            agent_id: "ceo".to_string(),
            text: "done".to_string(),
            steps: Vec::new(),
            task_id: Some("t-1".to_string()),
        };
        let back: CompanyEvent =
            serde_json::from_str(&serde_json::to_string(&tagged).unwrap()).unwrap();
        assert_eq!(back, tagged);

        // Same contract on the failure event.
        let legacy_mcp: CompanyEvent = serde_json::from_str(
            r#"{"kind":"McpCallFailed","server":"gh","tool":"issues","status":"credential_required","message":"needs auth"}"#,
        )
        .expect("a pre-task_id McpCallFailed still loads");
        match &legacy_mcp {
            CompanyEvent::McpCallFailed { task_id, .. } => assert!(task_id.is_none()),
            other => panic!("expected McpCallFailed, got {other:?}"),
        }
    }

    /// #185: the dispatch terminal round-trips, and reports where the card
    /// landed so a stopped run is distinguishable from a successful one.
    #[test]
    fn desk_task_completed_round_trips() {
        let done = CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".to_string(),
            desk: "ceo".to_string(),
            output: "shipped".to_string(),
            column: "in_review".to_string(),
            artifact_ids: Vec::new(),
            origin_chat_id: None,
        };
        let json = serde_json::to_string(&done).unwrap();
        assert!(json.contains(r#""kind":"DeskTaskCompleted""#));
        assert!(
            !json.contains("artifact_ids"),
            "a task that published nothing must add nothing to the log: {json}"
        );
        assert!(
            !json.contains("origin_chat_id"),
            "a board-created card names no conversation, so it must add nothing \
             to the log either: {json}"
        );
        let back: CompanyEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, done);
    }

    /// Issue #377: the terminal carries the conversation the card was raised
    /// from, and a line written before the field existed still replays as
    /// origin-less — which is the truth about it (nobody raised it from a chat
    /// that this log records), not a default standing in for a lost id.
    ///
    /// The legacy blob is asserted verbatim for the same reason #244's is: it
    /// is exactly what is already on disk in every company's event log. If this
    /// fails, the change needs a migration rather than a `#[serde(default)]`.
    #[test]
    fn desk_task_completed_carries_its_origin_chat_and_still_reads_the_old_shape() {
        let done = CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".to_string(),
            desk: "engineer".to_string(),
            output: "shipped".to_string(),
            column: "in_review".to_string(),
            artifact_ids: Vec::new(),
            origin_chat_id: Some("engineering".to_string()),
        };
        let json = serde_json::to_string(&done).unwrap();
        assert!(json.contains(r#""origin_chat_id":"engineering""#), "{json}");
        assert_eq!(
            serde_json::from_str::<CompanyEvent>(&json).unwrap(),
            done,
            "the origin must survive the round trip"
        );

        // The responder and the channel are different words on purpose — this
        // is why the origin has to be carried rather than derived from `desk`.
        assert!(
            !json.contains(r#""desk":"engineering""#),
            "the responder is not the channel: {json}"
        );

        let legacy = r#"{"kind":"DeskTaskCompleted","task_id":"t-1","desk":"ceo","output":"shipped","column":"in_review"}"#;
        assert_eq!(
            serde_json::from_str::<CompanyEvent>(legacy).unwrap(),
            CompanyEvent::DeskTaskCompleted {
                task_id: "t-1".to_string(),
                desk: "ceo".to_string(),
                output: "shipped".to_string(),
                column: "in_review".to_string(),
                artifact_ids: Vec::new(),
                origin_chat_id: None,
            },
            "a pre-#377 journal line must replay with no origin, not fail"
        );
    }

    /// Issue #244: the terminal anchor names what the run published, and a line
    /// written before the field existed still replays.
    ///
    /// The legacy blob is asserted verbatim because it is exactly what is
    /// already on disk in every company's event log — if this ever fails, the
    /// change needs a migration rather than a `#[serde(default)]`.
    #[test]
    fn desk_task_completed_carries_artifact_ids_and_still_reads_the_old_shape() {
        let done = CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".to_string(),
            desk: "ceo".to_string(),
            output: "Drafted the launch spec.".to_string(),
            column: "in_review".to_string(),
            artifact_ids: vec!["art-1".to_string(), "art-2".to_string()],
            origin_chat_id: None,
        };
        let json = serde_json::to_string(&done).unwrap();
        assert!(
            json.contains(r#""artifact_ids":["art-1","art-2"]"#),
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<CompanyEvent>(&json).unwrap(),
            done,
            "the ids must survive the round trip"
        );

        let legacy = r#"{"kind":"DeskTaskCompleted","task_id":"t-1","desk":"ceo","output":"shipped","column":"in_review"}"#;
        assert_eq!(
            serde_json::from_str::<CompanyEvent>(legacy).unwrap(),
            CompanyEvent::DeskTaskCompleted {
                task_id: "t-1".to_string(),
                desk: "ceo".to_string(),
                output: "shipped".to_string(),
                column: "in_review".to_string(),
                artifact_ids: Vec::new(),
                origin_chat_id: None,
            },
            "a pre-#244 journal line must replay with no artifacts, not fail"
        );
    }

    /// Issue #364: a thread parent round-trips, and a message journaled before
    /// threads existed still replays — as unparented, which is the truth about
    /// it and not a default standing in for one.
    ///
    /// The legacy blobs are asserted verbatim because they are exactly what is
    /// already on disk in every company's log. A message that never was a thread
    /// reply must serialize byte-for-byte as it always did, so export/import and
    /// the cross-backend round-trip need no migration.
    #[test]
    fn a_thread_parent_round_trips_and_a_pre_thread_line_still_loads() {
        for legacy in [
            r#"{"kind":"OperatorMessage","text":"hi"}"#,
            r#"{"kind":"AgentReply","chat_id":"main","agent_id":"ceo","text":"hi"}"#,
        ] {
            let event: CompanyEvent = serde_json::from_str(legacy).unwrap();
            match &event {
                CompanyEvent::OperatorMessage { parent, .. }
                | CompanyEvent::AgentReply { parent, .. } => assert!(
                    parent.is_none(),
                    "a pre-#364 line was never a thread reply: {legacy}"
                ),
                other => panic!("unexpected variant: {other:?}"),
            }
            assert_eq!(
                serde_json::to_string(&event).unwrap(),
                legacy,
                "an unparented message must serialize exactly as it did before"
            );
        }

        let threaded = CompanyEvent::OperatorMessage {
            parent: Some(EventSeq::new(41)),
            text: "a follow-up".into(),
            by: None,
            chat: Some("studio".into()),
            deliverable: None,
        };
        let json = serde_json::to_string(&threaded).unwrap();
        assert!(json.contains(r#""parent":41"#), "{json}");
        assert_eq!(
            serde_json::from_str::<CompanyEvent>(&json).unwrap(),
            threaded
        );

        let answered = CompanyEvent::AgentReply {
            parent: Some(EventSeq::new(41)),
            task_id: None,
            chat_id: "studio".into(),
            agent_id: "ceo".into(),
            text: "on it".into(),
            steps: Vec::new(),
        };
        let json = serde_json::to_string(&answered).unwrap();
        assert!(json.contains(r#""parent":41"#), "{json}");
        assert_eq!(
            serde_json::from_str::<CompanyEvent>(&json).unwrap(),
            answered
        );
    }

    /// Issue #364: a reaction round-trips, and an unattributed one adds no
    /// `by` key — the same additive contract every optional actor here keeps.
    #[test]
    fn a_reaction_round_trips() {
        let anonymous = CompanyEvent::ReactionToggled {
            message_seq: EventSeq::new(4),
            emoji: "👍".into(),
            on: true,
            by: None,
        };
        let json = serde_json::to_string(&anonymous).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"ReactionToggled","message_seq":4,"emoji":"👍","on":true}"#
        );
        assert_eq!(
            serde_json::from_str::<CompanyEvent>(&json).unwrap(),
            anonymous
        );

        let attributed = CompanyEvent::ReactionToggled {
            message_seq: EventSeq::new(4),
            emoji: "🎉".into(),
            on: false,
            by: Some(Actor {
                kind: ActorKind::User,
                id: "u1".into(),
            }),
        };
        let json = serde_json::to_string(&attributed).unwrap();
        assert_eq!(
            serde_json::from_str::<CompanyEvent>(&json).unwrap(),
            attributed
        );
    }

    /// The three intents are exactly the three wire words, and only those
    /// (issue #1152).
    ///
    /// Pinned as literals because the console and the journal both write them:
    /// a rename here silently stops matching every message already on disk.
    #[test]
    fn a_message_intent_round_trips_through_its_wire_word() {
        for (intent, word) in [
            (MessageIntent::Chat, "chat"),
            (MessageIntent::Once, "once"),
            (MessageIntent::Workflow, "workflow"),
        ] {
            let json = serde_json::to_string(&intent).unwrap();
            assert_eq!(json, format!("\"{word}\""));
            assert_eq!(
                serde_json::from_str::<MessageIntent>(&json).unwrap(),
                intent
            );
            assert_eq!(intent.as_str(), word);
        }
        assert!(
            serde_json::from_str::<MessageIntent>(r#""build""#).is_err(),
            "the set is closed: an unknown word is a 400, not a silent default"
        );
    }

    /// "Just chatting" has no deliverable, and that is the whole point of the
    /// type (issue #1152).
    ///
    /// A card can never *be* "not work", so the honest mapping from a `Chat`
    /// message onto the card field is "there is no card" — `None` — rather than
    /// a third `TaskDeliverable` variant every stored reader would owe a branch
    /// for.
    #[test]
    fn only_a_work_intent_maps_onto_a_card_deliverable() {
        use crate::ports::tasks::TaskDeliverable;

        assert_eq!(MessageIntent::Chat.deliverable(), None);
        assert_eq!(
            MessageIntent::Once.deliverable(),
            Some(TaskDeliverable::Once)
        );
        assert_eq!(
            MessageIntent::Workflow.deliverable(),
            Some(TaskDeliverable::Workflow)
        );
        assert!(MessageIntent::Chat.is_chat());
        assert!(!MessageIntent::Once.is_chat());
        assert!(!MessageIntent::Workflow.is_chat());
    }

    /// **No journaled record migrates** (issue #1152).
    ///
    /// Retyping `OperatorMessage::deliverable` from `TaskDeliverable` to
    /// [`MessageIntent`] is only safe if every value already written under that
    /// key still loads, and still writes back the same bytes. Getting this wrong
    /// does not fail CI — it fails on somebody's event log, on whichever of the
    /// three backends they run, the next time a company boots. So the claim is a
    /// test rather than a sentence in a doc comment.
    ///
    /// The blobs are asserted verbatim in both directions: parsed to the value
    /// the new type gives them, and re-serialized byte-for-byte back to what is
    /// on disk.
    #[test]
    fn every_journaled_deliverable_value_still_loads_and_writes_back_identically() {
        for (blob, expected) in [
            (r#"{"kind":"OperatorMessage","text":"hi"}"#, None),
            (
                r#"{"kind":"OperatorMessage","text":"ship the landing page","deliverable":"once"}"#,
                Some(MessageIntent::Once),
            ),
            (
                r#"{"kind":"OperatorMessage","text":"build me a weekly report","deliverable":"workflow"}"#,
                Some(MessageIntent::Workflow),
            ),
        ] {
            let event: CompanyEvent = serde_json::from_str(blob).unwrap_or_else(|e| {
                panic!("a stored line must still load: {blob} — {e}");
            });
            match &event {
                CompanyEvent::OperatorMessage { deliverable, .. } => {
                    assert_eq!(*deliverable, expected, "{blob}")
                }
                other => panic!("unexpected variant: {other:?}"),
            }
            assert_eq!(
                serde_json::to_string(&event).unwrap(),
                blob,
                "a stored line must serialize back byte-for-byte"
            );
        }
    }

    /// The new word travels on the same key, and only when it was chosen
    /// (issue #1152).
    ///
    /// The absent case is the compatibility half that matters most: "Do it
    /// once" is not the default *because it is sent* — it is the default
    /// because nothing is sent, so an unmarked message is byte-identical on the
    /// wire to every message journaled before this control existed.
    #[test]
    fn a_chat_intent_journals_under_the_same_key_and_absence_stays_absent() {
        let chatting = CompanyEvent::OperatorMessage {
            text: "morning all".into(),
            by: None,
            chat: None,
            parent: None,
            deliverable: Some(MessageIntent::Chat),
        };
        assert_eq!(
            serde_json::to_string(&chatting).unwrap(),
            r#"{"kind":"OperatorMessage","text":"morning all","deliverable":"chat"}"#
        );
        assert_eq!(
            serde_json::from_str::<CompanyEvent>(
                r#"{"kind":"OperatorMessage","text":"morning all","deliverable":"chat"}"#
            )
            .unwrap(),
            chatting
        );

        let unmarked = CompanyEvent::OperatorMessage {
            text: "morning all".into(),
            by: None,
            chat: None,
            parent: None,
            deliverable: None,
        };
        assert_eq!(
            serde_json::to_string(&unmarked).unwrap(),
            r#"{"kind":"OperatorMessage","text":"morning all"}"#,
            "no choice must still put nothing on the wire"
        );
    }

    #[test]
    fn an_operator_message_journaled_before_attribution_still_loads() {
        // Exactly what is already on disk in every existing company's event
        // log. If this ever fails, the change needs a migration.
        let legacy = r#"{"kind":"OperatorMessage","text":"hi"}"#;
        let event: CompanyEvent = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            event,
            CompanyEvent::OperatorMessage {
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
                deliverable: None,
            }
        );
    }

    #[test]
    fn an_unattributed_message_serializes_exactly_as_it_did_before() {
        // `skip_serializing_if` keeps the old bytes. This is what lets
        // export/import and the fs/sqlite/mongo round-trip stay green without
        // touching a single stored record.
        let event = CompanyEvent::OperatorMessage {
            parent: None,
            text: "hi".into(),
            by: None,
            chat: None,
            deliverable: None,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"kind":"OperatorMessage","text":"hi"}"#
        );
    }

    #[test]
    fn an_attributed_message_round_trips_with_its_actor() {
        let event = CompanyEvent::OperatorMessage {
            parent: None,
            text: "hi".into(),
            by: Some(Actor {
                kind: ActorKind::User,
                id: "u1".into(),
            }),
            chat: None,
            deliverable: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["by"]["kind"], "user");
        assert_eq!(json["by"]["id"], "u1");
        assert_eq!(serde_json::from_value::<CompanyEvent>(json).unwrap(), event);
    }

    #[test]
    fn actor_kind_is_still_copy() {
        // A `String`-carrying variant would have taken this away from every
        // existing holder, which is why the User id lives on `Actor` instead.
        fn assert_copy(kind: ActorKind) -> (ActorKind, ActorKind) {
            (kind, kind)
        }
        let (a, b) = assert_copy(ActorKind::User);
        assert_eq!(a, b);
    }

    #[test]
    fn company_event_variants_round_trip_tagged() {
        let events = vec![
            CompanyEvent::OperatorMessage {
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
                deliverable: None,
            },
            CompanyEvent::WebhookReceived {
                channel: "email".into(),
                body: serde_json::json!({"subject": "hello"}),
            },
            CompanyEvent::ScheduleFired {
                cron: "0 9 * * *".into(),
                prompt: "daily standup".into(),
            },
            CompanyEvent::A2aTaskReceived {
                from: "@peer".into(),
                task: serde_json::json!({"skill": "seo.audit"}),
            },
            CompanyEvent::ApprovalResolved {
                approval_id: ApprovalId::new("a1"),
                verdict: Verdict::Approve,
                by: Actor {
                    kind: ActorKind::Operator,
                    id: "owner".into(),
                },
            },
            CompanyEvent::FeedbackFiled {
                note: "too slow".into(),
            },
            CompanyEvent::PaymentReceived {
                amount_usd: 25.0,
                memo: "invoice #1".into(),
            },
        ];
        for event in &events {
            assert_eq!(&round_trip(event), event);
        }

        // The tag field is emitted under `kind`.
        let json = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(json["kind"], "OperatorMessage");
        assert_eq!(json["text"], "hi");
    }

    #[test]
    fn mcp_call_failed_round_trips_and_is_byte_stable() {
        let event = CompanyEvent::McpCallFailed {
            task_id: None,
            server: "browserbase".into(),
            tool: "browse".into(),
            status: "tool_call_rejected".into(),
            message: "server rejected the call".into(),
        };
        assert_eq!(round_trip(&event), event);
        // The tag is emitted under `kind`, and the field set is fixed — a byte
        // guard so a later field addition is a deliberate, tested change.
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"kind":"McpCallFailed","server":"browserbase","tool":"browse","status":"tool_call_rejected","message":"server rejected the call"}"#
        );
    }

    #[test]
    fn task_steered_round_trips_and_omits_empty_fields() {
        // A plain pause: no `instruction`, no `by` — both must be OMITTED from
        // the wire (skip_serializing_if), so old logs stay byte-stable.
        let pause = CompanyEvent::TaskSteered {
            task_id: "t1".into(),
            action: "pause".into(),
            instruction: None,
            by: None,
        };
        assert_eq!(round_trip(&pause), pause);
        assert_eq!(
            serde_json::to_string(&pause).unwrap(),
            r#"{"kind":"TaskSteered","task_id":"t1","action":"pause"}"#
        );

        // A redirect carries its (capped) instruction; still no actor.
        let redirect = CompanyEvent::TaskSteered {
            task_id: "t1".into(),
            action: "redirect".into(),
            instruction: Some("focus on the API".into()),
            by: None,
        };
        assert_eq!(round_trip(&redirect), redirect);
        assert_eq!(
            serde_json::to_string(&redirect).unwrap(),
            r#"{"kind":"TaskSteered","task_id":"t1","action":"redirect","instruction":"focus on the API"}"#
        );
    }

    /// Issue #335: an unattributed post must serialize with **no** `by` key, so
    /// the variant's wire shape is the same one a machine-credentialled post
    /// wrote before attribution could ever be present — and an attributed one
    /// round-trips its actor.
    #[test]
    fn task_discussion_posted_round_trips_and_omits_an_absent_actor() {
        let anonymous = CompanyEvent::TaskDiscussionPosted {
            task_id: "t1".into(),
            text: "blocked on the API key".into(),
            by: None,
        };
        assert_eq!(round_trip(&anonymous), anonymous);
        assert_eq!(
            serde_json::to_string(&anonymous).unwrap(),
            r#"{"kind":"TaskDiscussionPosted","task_id":"t1","text":"blocked on the API key"}"#
        );

        let attributed = CompanyEvent::TaskDiscussionPosted {
            task_id: "t1".into(),
            text: "unblocked".into(),
            by: Some(Actor {
                kind: ActorKind::User,
                id: "u-7".into(),
            }),
        };
        assert_eq!(round_trip(&attributed), attributed);
    }

    /// Issue #358: the tombstone's wire shape, pinned because it is written
    /// into `events.jsonl` and read back by a *different* instance on import.
    /// The pair (post, tombstone) is what stops a withdrawn message being
    /// resurrected, so a tombstone that failed to round-trip would silently
    /// restore the text it was appended to remove.
    #[test]
    fn task_discussion_redacted_round_trips_and_omits_an_absent_actor() {
        let anonymous = CompanyEvent::TaskDiscussionRedacted {
            task_id: "t1".into(),
            seq: 42,
            by: None,
        };
        assert_eq!(round_trip(&anonymous), anonymous);
        assert_eq!(
            serde_json::to_string(&anonymous).unwrap(),
            r#"{"kind":"TaskDiscussionRedacted","task_id":"t1","seq":42}"#
        );

        let attributed = CompanyEvent::TaskDiscussionRedacted {
            task_id: "t1".into(),
            seq: 42,
            by: Some(Actor {
                kind: ActorKind::User,
                id: "u-7".into(),
            }),
        };
        assert_eq!(round_trip(&attributed), attributed);
    }

    #[test]
    fn verdict_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Verdict::Approve).unwrap(),
            "\"approve\""
        );
        assert_eq!(serde_json::to_string(&Verdict::Deny).unwrap(), "\"deny\"");
        assert_eq!(
            serde_json::from_str::<Verdict>("\"approve\"").unwrap(),
            Verdict::Approve
        );
    }

    #[test]
    fn effect_round_trips_and_accessors_read_fields() {
        let effect = Effect {
            kind: "payment.send".into(),
            group: EffectGroup::Spend,
            amount_usd: Some(42.5),
            established_thread: true,
            first_time_counterparty: false,
            payload: serde_json::json!({"to": "@vendor"}),
            agent: None,
            run_id: None,
        };
        let back = round_trip(&effect);
        assert_eq!(back, effect);
        assert_eq!(effect.kind(), "payment.send");
        assert_eq!(effect.group(), EffectGroup::Spend);
        assert_eq!(effect.amount_usd(), Some(42.5));
        assert!(effect.is_established_thread());
        assert!(!effect.is_first_time_counterparty());
    }

    #[test]
    fn effect_disposition_round_trips() {
        for disp in [
            EffectDisposition::Executed,
            EffectDisposition::PendingApproval(ApprovalId::new("x")),
            EffectDisposition::Denied {
                reason: "over cap".into(),
            },
        ] {
            assert_eq!(round_trip(&disp), disp);
        }
    }

    #[test]
    fn policy_decision_round_trips() {
        for dec in [
            PolicyDecision::Allow,
            PolicyDecision::RequireApproval,
            PolicyDecision::Deny,
        ] {
            assert_eq!(round_trip(&dec), dec);
        }
    }

    #[test]
    fn event_seq_orders_numerically() {
        assert!(EventSeq::new(1) < EventSeq::new(2));
        assert_eq!(EventSeq::new(7).value(), 7);
    }

    #[test]
    fn agent_card_round_trips_with_extended_fields() {
        let card = AgentCard {
            handle: "acme".into(),
            description: "We audit SEO.".into(),
            skills: vec!["seo.audit".into()],
            name: "Acme SEO".into(),
            actor_type: "agent".into(),
            endpoint: "https://host/a2a/acme".into(),
            supported_interfaces: vec!["a2a-jsonrpc".into()],
            capabilities: vec!["seo.audit".into()],
            tags: vec!["seo.audit".into()],
            payment_requirements: vec![CardPayment {
                skill_id: "seo.audit".into(),
                price: "25.00".into(),
                asset: "USDC".into(),
                network: "solana".into(),
            }],
        };
        assert_eq!(round_trip(&card), card);
    }

    fn desk_record(toml_src: &str, overlay: Vec<OverlayDeskMember>) -> CompanyRecord {
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest: toml::from_str(toml_src).expect("parse manifest"),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: overlay,
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
        }
    }

    /// Like [`desk_record`] but with an explicit per-desk order overlay, for the
    /// desk-hierarchy tests.
    fn desk_record_ordered(
        toml_src: &str,
        overlay: Vec<OverlayDeskMember>,
        order: Vec<OverlayDeskOrder>,
    ) -> CompanyRecord {
        let mut record = desk_record(toml_src, overlay);
        record.overlay_desk_order = order;
        record
    }

    /// The effective membership is the manifest members first, then overlay
    /// additions in insertion order, deduplicated — the shared rule the REST
    /// list and the harness desk-lead resolver both read.
    #[test]
    fn effective_desk_members_unions_manifest_and_overlay_deduped() {
        let manifest = "[company]\nname = \"Acme\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n\
             [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\nmembers = [\"ceo\"]\n";
        let record = desk_record(
            manifest,
            vec![
                OverlayDeskMember {
                    desk_id: "studio".into(),
                    agent_id: "eng".into(),
                },
                // A duplicate of a manifest member is not added twice.
                OverlayDeskMember {
                    desk_id: "studio".into(),
                    agent_id: "ceo".into(),
                },
                // An addition for a different desk is ignored here.
                OverlayDeskMember {
                    desk_id: "other".into(),
                    agent_id: "eng".into(),
                },
            ],
        );
        assert_eq!(
            record.effective_desk_members("studio"),
            vec!["ceo".to_string(), "eng".to_string()]
        );
        // An unknown desk with only an overlay addition still resolves it.
        assert_eq!(
            record.effective_desk_members("other"),
            vec!["eng".to_string()]
        );
    }

    /// A three-member manifest desk whose order override permutes the members.
    const HIERARCHY_MANIFEST: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n\
         [[agent]]\nid = \"des\"\nrole = \"Designer\"\n\
         [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\nmembers = [\"ceo\", \"eng\", \"des\"]\n";

    fn order(desk: &str, ids: &[&str]) -> Vec<OverlayDeskOrder> {
        vec![OverlayDeskOrder {
            desk_id: desk.into(),
            ordered: ids.iter().map(|s| s.to_string()).collect(),
        }]
    }

    /// A full permutation reorders the manifest members exactly as given.
    #[test]
    fn desk_order_reorders_manifest_members() {
        let record = desk_record_ordered(
            HIERARCHY_MANIFEST,
            Vec::new(),
            order("studio", &["des", "ceo", "eng"]),
        );
        assert_eq!(
            record.effective_desk_members("studio"),
            vec!["des".to_string(), "ceo".to_string(), "eng".to_string()]
        );
    }

    /// The override can promote an overlay-added member above manifest members —
    /// the whole-set permutation a per-member rank could not express.
    #[test]
    fn desk_order_promotes_overlay_member_to_lead() {
        let record = desk_record_ordered(
            HIERARCHY_MANIFEST,
            vec![OverlayDeskMember {
                desk_id: "studio".into(),
                agent_id: "cto".into(),
            }],
            order("studio", &["cto", "ceo", "eng", "des"]),
        );
        let members = record.effective_desk_members("studio");
        assert_eq!(members[0], "cto");
        assert_eq!(
            members,
            vec![
                "cto".to_string(),
                "ceo".to_string(),
                "eng".to_string(),
                "des".to_string()
            ]
        );
    }

    /// An absent or empty override reproduces the base order byte-for-byte.
    #[test]
    fn desk_order_absent_or_empty_keeps_base_order() {
        let base = desk_record(HIERARCHY_MANIFEST, Vec::new());
        let base_members = base.effective_desk_members("studio");
        assert_eq!(base_members, vec!["ceo", "eng", "des"]);

        // An explicit empty override for the desk is a no-op too.
        let empty = desk_record_ordered(HIERARCHY_MANIFEST, Vec::new(), order("studio", &[]));
        assert_eq!(empty.effective_desk_members("studio"), base_members);
    }

    /// Ids in the override that are no longer desk members are ignored.
    #[test]
    fn desk_order_ignores_stale_ids() {
        let record = desk_record_ordered(
            HIERARCHY_MANIFEST,
            Vec::new(),
            order("studio", &["ghost", "des", "ceo", "eng"]),
        );
        // `ghost` is not a member, so it contributes nothing; the rest apply.
        assert_eq!(
            record.effective_desk_members("studio"),
            vec!["des".to_string(), "ceo".to_string(), "eng".to_string()]
        );
    }

    /// A subset override lists its ids first, then the unlisted members keep
    /// their base relative order after.
    #[test]
    fn desk_order_subset_is_listed_first_then_default() {
        let record = desk_record_ordered(HIERARCHY_MANIFEST, Vec::new(), order("studio", &["des"]));
        // `des` promoted first; `ceo`, `eng` keep their base order behind it.
        assert_eq!(
            record.effective_desk_members("studio"),
            vec!["des".to_string(), "ceo".to_string(), "eng".to_string()]
        );
    }

    /// The persisted overlay blob round-trips the desk-order collection.
    #[test]
    fn overlay_blob_round_trips_desk_order() {
        let record = desk_record_ordered(
            HIERARCHY_MANIFEST,
            Vec::new(),
            order("studio", &["des", "ceo", "eng"]),
        );
        let blob = OverlayBlob::from_record(&record);
        let json = serde_json::to_string(&blob).expect("serialize blob");
        let parsed = OverlayBlob::parse(&json).expect("parse blob");
        assert_eq!(parsed.desk_order, record.overlay_desk_order);
    }

    /// The persisted overlay blob round-trips the `[policy]` override, and a
    /// blob written before it existed still parses (issue #562).
    ///
    /// Both halves matter. Without the first, a serialization path that dropped
    /// the field would move an operator's approval gate back to the manifest on
    /// the next load, silently. Without the second, every company record written
    /// before this feature would fail to parse at all.
    #[test]
    fn overlay_blob_round_trips_the_policy_override() {
        let mut record = desk_record(POLICY_MANIFEST, Vec::new());
        record.overlay_policy = Some(policy_entry(Some("auto"), Some(vec!["payment.send"])));

        let blob = OverlayBlob::from_record(&record);
        let json = serde_json::to_string(&blob).expect("serialize blob");
        let parsed = OverlayBlob::parse(&json).expect("parse blob");
        assert_eq!(parsed.policy, record.overlay_policy);

        // A blob from before this field existed loads as "not overridden",
        // which is the pre-#562 behaviour exactly.
        let legacy = r#"{"agents":[],"desk_members":[],"budgets":[]}"#;
        let blob = OverlayBlob::parse(legacy).expect("blob without a policy key");
        assert!(
            blob.policy.is_none(),
            "an older record must load with the manifest's policy in charge"
        );

        // And so does the oldest form of all, the bare agent array.
        let bare = OverlayBlob::parse("[]").expect("legacy array");
        assert!(bare.policy.is_none());
    }

    /// An object-form blob written before `desk_order` existed still parses, and
    /// the legacy bare-array form still parses — both with an empty order.
    #[test]
    fn overlay_blob_parses_without_desk_order_key() {
        // Object form missing the `desk_order` key (pre-#131 rows).
        let object = r#"{"agents":[{"id":"a","name":"A","role":"r"}],"desk_members":[{"desk_id":"d","agent_id":"a"}]}"#;
        let blob = OverlayBlob::parse(object).expect("object without desk_order");
        assert_eq!(blob.desk_members.len(), 1);
        assert!(blob.desk_order.is_empty());

        // Legacy bare `Vec<OverlayAgent>` form.
        let legacy = r#"[{"id":"a","name":"A","role":"r"}]"#;
        let blob = OverlayBlob::parse(legacy).expect("legacy array");
        assert!(blob.desk_order.is_empty());
    }

    /// `is_roster_agent` accepts both manifest agents and overlay teammates, and
    /// rejects an unknown id — the validation the desk-add route relies on.
    #[test]
    fn is_roster_agent_covers_manifest_and_overlay() {
        let manifest = "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";
        let mut record = desk_record(manifest, Vec::new());
        record.overlay_agents.push(OverlayAgent {
            id: "nova".into(),
            name: "Nova".into(),
            role: "Growth".into(),
            description: None,
            tools: Vec::new(),
        });
        assert!(record.is_roster_agent("ceo"));
        assert!(record.is_roster_agent("nova"));
        assert!(!record.is_roster_agent("ghost"));
    }

    /// Issue #661 / L5 serde: the new per-teammate `tools` grant is optional and
    /// empty-by-default on the wire, so an overlay record written before the
    /// field existed deserializes unchanged, and a standard-grant teammate
    /// serializes exactly as it did before (no `tools` key).
    #[test]
    fn overlay_agent_tools_defaults_empty_and_skips_when_empty() {
        // An old record with no `tools` key deserializes to an empty grant.
        let legacy: OverlayAgent =
            serde_json::from_str(r#"{"id":"a","name":"A","role":"r"}"#).expect("legacy overlay");
        assert!(legacy.tools.is_empty());

        // An empty grant is omitted from the serialized form — a standard-grant
        // teammate is byte-for-byte what it was before this field existed.
        let value = serde_json::to_value(&legacy).unwrap();
        assert!(
            value.get("tools").is_none(),
            "an empty grant must not serialize a `tools` key: {value}"
        );

        // A non-empty grant round-trips in order.
        let scoped = OverlayAgent {
            id: "s".into(),
            name: "S".into(),
            role: "r".into(),
            description: None,
            tools: vec!["docs.*".into(), "email".into()],
        };
        let round: OverlayAgent =
            serde_json::from_str(&serde_json::to_string(&scoped).unwrap()).unwrap();
        assert_eq!(round.tools, vec!["docs.*".to_string(), "email".to_string()]);
    }

    /// A record with one manifest agent and no desks, for the minting tests.
    fn mint_record() -> CompanyRecord {
        desk_record(
            "[company]\nname = \"Acme\"\n\
             [[agent]]\nid = \"backend_engineer\"\nrole = \"Backend Engineer\"\n",
            Vec::new(),
        )
    }

    fn add_overlay(record: &mut CompanyRecord, id: &str, name: &str) {
        record.overlay_agents.push(OverlayAgent {
            id: id.into(),
            name: name.into(),
            role: "Worker".into(),
            description: None,
            tools: Vec::new(),
        });
    }

    /// A free slug is minted bare — the whole point of issue #686 is that the
    /// common case reads as `Agents/dana_designer/`.
    #[test]
    fn mint_agent_id_takes_the_bare_slug_when_it_is_free() {
        let record = mint_record();
        assert_eq!(record.mint_agent_id("Dana Designer"), "dana_designer");
        assert_eq!(record.mint_agent_id("Designer!!"), "designer");
        assert_eq!(record.mint_agent_id("24/7 Support"), "teammate");
    }

    /// The collision that matters: an overlay id equal to a **manifest** id is
    /// skipped by `build_roster`, so the teammate would save and never
    /// materialise. Suffixing is what keeps it reachable.
    #[test]
    fn mint_agent_id_suffixes_past_a_manifest_agent() {
        let record = mint_record();
        assert_eq!(
            record.mint_agent_id("Backend Engineer"),
            "backend_engineer_2"
        );
    }

    /// Repeated adds of one name walk `_2`, `_3`, … in order, so the ids a
    /// company ends up with are a function of its roster and not of arrival
    /// timing.
    #[test]
    fn mint_agent_id_walks_suffixes_deterministically() {
        let mut record = mint_record();
        let first = record.mint_agent_id("Designer");
        assert_eq!(first, "designer");
        add_overlay(&mut record, &first, "Designer");

        let second = record.mint_agent_id("Designer");
        assert_eq!(second, "designer_2");
        add_overlay(&mut record, &second, "Designer");

        assert_eq!(record.mint_agent_id("Designer"), "designer_3");

        // A degenerate name is not a special case — it suffixes like any other.
        add_overlay(&mut record, "teammate", "***");
        assert_eq!(record.mint_agent_id("🙂"), "teammate_2");
    }

    /// Case is not a difference: an overlay id typed with capitals still blocks
    /// the lowercase slug, because `resolve_roster_agent_id` folds case and two
    /// teammates one capital apart would be one unroutable key.
    #[test]
    fn mint_agent_id_treats_a_case_variant_id_as_taken() {
        let mut record = mint_record();
        add_overlay(&mut record, "Dana_Designer", "Dana Designer");
        assert_eq!(record.mint_agent_id("Dana Designer"), "dana_designer_2");
    }

    /// Desks resolve *before* teammates in `assignee::resolve`, by id and by
    /// case-insensitive display name — so a minted id equal to either would be
    /// unreachable, and both are stepped past.
    #[test]
    fn mint_agent_id_steps_past_desk_ids_and_desk_names() {
        let manifest = "[company]\nname = \"Acme\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[group_chat]]\nid = \"growth\"\nname = \"Content\"\nmembers = [\"ceo\"]\n";
        let mut record = desk_record(manifest, Vec::new());

        // By desk id.
        assert_eq!(record.mint_agent_id("Growth"), "growth_2");
        // By desk display name, which `resolve_desk_id` matches ignoring case.
        assert_eq!(record.mint_agent_id("content"), "content_2");

        // A desk name that is not itself slug-shaped is *not* reserved: nothing
        // routes on the slug of a desk name, only on the name as written, so
        // `content_desk` shadows no key that "Content Desk" answers to.
        record.overlay_desks.push(OverlayDesk {
            id: "design".into(),
            name: "Design Studio".into(),
            description: None,
            members: Vec::new(),
        });
        assert_eq!(record.mint_agent_id("Design Studio"), "design_studio");
        // …while the overlay desk's id is reserved exactly like a manifest one.
        assert_eq!(record.mint_agent_id("Design"), "design_2");
    }

    /// The operator channel and the workspace system roots are never handed to
    /// a teammate, on an otherwise empty roster.
    #[test]
    fn mint_agent_id_never_returns_a_reserved_id() {
        let record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
        assert_eq!(record.mint_agent_id("Operator"), "operator_2");
        assert_eq!(record.mint_agent_id("Agents"), "agents_2");
        assert_eq!(record.mint_agent_id("desks"), "desks_2");
        assert_eq!(record.mint_agent_id("System"), "system_2");
        assert_eq!(
            RESERVED_AGENT_IDS,
            ["operator", "Agents", "Desks", "system"]
        );
    }

    /// Issue #966: the host's own author is not a name a teammate can be given.
    ///
    /// `SYSTEM_AUTHOR` reaches the console's centred system pill by value —
    /// `MessageView` projects an `AgentReply`'s `agent_id` straight into
    /// `author`, and the console keys on the string. A teammate holding that id
    /// would therefore render *as the host*, which is a worse confusion than the
    /// one this issue set out to fix, and the value it replaces (`"operator"`)
    /// was already reserved.
    ///
    /// Its sibling `CONFINED_AGENT_ID` needs no entry here: `agent_slug` emits
    /// only lowercase alphanumerics and underscores, so `"workflow-copilot"` is
    /// unmintable by construction. `"system"` is an ordinary legal slug.
    #[test]
    fn mint_agent_id_never_returns_the_host_author() {
        let record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
        assert_eq!(
            agent_slug("System"),
            crate::ports::SYSTEM_AUTHOR,
            "the guard is needed precisely because this is a legal slug"
        );
        assert_ne!(
            record.mint_agent_id("System"),
            crate::ports::SYSTEM_AUTHOR,
            "a teammate must never be minted onto the id the runtime speaks under"
        );
    }

    /// Issue #1162: the resolve every surface that takes a teammate key runs.
    /// An id resolves, an overlay teammate's **display name** resolves to the
    /// id it was minted under, and a key that is nobody resolves to nothing.
    #[test]
    fn resolve_teammate_key_takes_an_id_or_a_display_name() {
        let manifest = "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";
        let mut record = desk_record(manifest, Vec::new());
        record.overlay_agents.push(OverlayAgent {
            id: "dana_designer".into(),
            name: "Dana Designer".into(),
            role: "Designer".into(),
            description: None,
            tools: Vec::new(),
        });

        assert_eq!(
            record.resolve_teammate_key("ceo"),
            TeammateResolution::Agent("ceo".into())
        );
        assert_eq!(
            record.resolve_teammate_key("dana_designer"),
            TeammateResolution::Agent("dana_designer".into())
        );
        // The case #1162 is about: the name `query_company` prints, grounding
        // to the id the delegation tools accept.
        assert_eq!(
            record.resolve_teammate_key("Dana Designer"),
            TeammateResolution::Agent("dana_designer".into())
        );
        assert_eq!(
            record.resolve_teammate_key("  dana designer  "),
            TeammateResolution::Agent("dana_designer".into())
        );
        assert_eq!(
            record.resolve_teammate_key("ghost"),
            TeammateResolution::Unknown
        );
        assert_eq!(
            record.resolve_teammate_key("   "),
            TeammateResolution::Unknown
        );
    }

    /// Ids win. A teammate whose **display name** is another teammate's id can
    /// never intercept work meant for that id — the ordering is the guarantee
    /// that makes one shared resolver safe to use everywhere.
    #[test]
    fn resolve_teammate_key_never_lets_a_name_shadow_an_id() {
        let manifest = "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";
        let mut record = desk_record(manifest, Vec::new());
        record.overlay_agents.push(OverlayAgent {
            id: "impostor".into(),
            name: "ceo".into(),
            role: "Growth".into(),
            description: None,
            tools: Vec::new(),
        });
        assert_eq!(
            record.resolve_teammate_key("ceo"),
            TeammateResolution::Agent("ceo".into())
        );
    }

    /// Two teammates answering to one display name is a collision the operator
    /// created, and it is reported as one: every colliding id comes back, so a
    /// caller can name them instead of silently taking the first.
    #[test]
    fn resolve_teammate_key_reports_a_name_two_teammates_answer_to() {
        let mut record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
        for id in ["dana_designer", "dana_designer_2"] {
            record.overlay_agents.push(OverlayAgent {
                id: id.into(),
                name: "Dana Designer".into(),
                role: "Designer".into(),
                description: None,
                tools: Vec::new(),
            });
        }
        assert_eq!(
            record.resolve_teammate_key("dana designer"),
            TeammateResolution::Ambiguous(vec!["dana_designer".into(), "dana_designer_2".into()])
        );
        // Either id still resolves on its own — the collision is in the name.
        assert_eq!(
            record.resolve_teammate_key("dana_designer_2"),
            TeammateResolution::Agent("dana_designer_2".into())
        );
    }

    /// Whatever is minted is a legal roster id, suffix included — the same
    /// grammar the manifest validator holds a hand-authored id to.
    #[test]
    fn every_minted_id_satisfies_the_manifest_id_grammar() {
        let mut record = mint_record();
        for name in [
            "Dana Designer",
            "Backend Engineer",
            "***",
            "24/7 Support",
            "Operator",
            "設計者",
        ] {
            let id = record.mint_agent_id(name);
            assert!(
                crate::company::is_snake_case(&id),
                "minted id {id:?} from {name:?} is not a legal roster id"
            );
            add_overlay(&mut record, &id, name);
        }
    }

    /// The persisted overlay blob reads both the current object form and the
    /// legacy bare-`overlay_agents`-array form, so existing sqlite/mongo rows
    /// load without a migration.
    #[test]
    fn overlay_blob_parses_object_and_legacy_array() {
        let object = r#"{"agents":[{"id":"a","name":"A","role":"r"}],"desk_members":[{"desk_id":"d","agent_id":"a"}]}"#;
        let blob = OverlayBlob::parse(object).expect("object");
        assert_eq!(blob.agents.len(), 1);
        assert_eq!(blob.desk_members.len(), 1);
        // Issue #85: an object written before provenance existed omits the key;
        // `#[serde(default)]` loads it as `None` (zero-migration back-compat).
        assert!(blob.provenance.is_none());

        // Legacy: overlay_json used to hold a bare Vec<OverlayAgent>.
        let legacy = r#"[{"id":"a","name":"A","role":"r"}]"#;
        let blob = OverlayBlob::parse(legacy).expect("legacy array");
        assert_eq!(blob.agents.len(), 1);
        assert!(blob.desk_members.is_empty());
        assert!(blob.provenance.is_none());

        // The empty-array default persisted by fresh schema.
        let blob = OverlayBlob::parse("[]").expect("empty array");
        assert!(blob.agents.is_empty());
        assert!(blob.desk_members.is_empty());
        assert!(blob.provenance.is_none());
        assert!(blob.desks.is_empty());

        // A pre-desk-creation object row (no `desks` key) loads with an empty
        // desk overlay — no migration needed.
        let pre_desks = r#"{"agents":[],"desk_members":[]}"#;
        let blob = OverlayBlob::parse(pre_desks).expect("pre-desks object");
        assert!(blob.desks.is_empty());
    }

    /// Issue #85: a record's template provenance round-trips through the
    /// `OverlayBlob` the sqlite/mongodb stores persist, and a blob carrying
    /// provenance re-parses with it intact.
    #[test]
    fn overlay_blob_carries_template_provenance() {
        let mut record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
        record.template_provenance = Some(TemplateProvenance {
            source_id: "agentic_law_firm".to_string(),
            version: Some("2.0.0".to_string()),
            path: Some("companies/agentic_law_firm".to_string()),
        });
        let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
        let blob = OverlayBlob::parse(&json).expect("reparse");
        assert_eq!(blob.provenance, record.template_provenance);
    }

    /// Issue #168: a runtime-authored workflow body round-trips through the
    /// `OverlayBlob` the sqlite/mongodb stores persist as `overlay_json`. On a
    /// hosted tenant this blob is the ONLY copy of the graph, so a serialization
    /// gap here would silently delete the workflow.
    #[test]
    fn overlay_blob_round_trips_workflows() {
        let mut record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
        record.overlay_workflows.push(OverlayWorkflow {
            id: "greeter".to_string(),
            toml: "id = \"greeter\"\nname = \"Greeter\"\n".to_string(),
        });
        let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
        let blob = OverlayBlob::parse(&json).expect("reparse");
        assert_eq!(blob.workflows, record.overlay_workflows);

        // A row written before workflow bodies persisted (no `workflows` key)
        // loads as empty — no migration needed.
        let legacy = r#"{"agents":[],"desk_members":[]}"#;
        assert!(
            OverlayBlob::parse(legacy)
                .expect("pre-workflows object")
                .workflows
                .is_empty()
        );
        // …and so does the legacy bare-array form.
        assert!(
            OverlayBlob::parse("[]")
                .expect("legacy array")
                .workflows
                .is_empty()
        );
    }

    /// Issue #276: the paused-workflow ids ride the same overlay blob as the
    /// graph bodies, reconstructed on load by both string-column stores
    /// (`sqlite` and `mongodb` read `OverlayBlob::parse`). A round trip here
    /// pins that the field is not dropped in `from_record`/`parse`.
    #[test]
    fn overlay_blob_round_trips_disabled_workflows() {
        let mut record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
        record.disabled_workflows.push("digest".to_string());
        let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
        let blob = OverlayBlob::parse(&json).expect("reparse");
        assert_eq!(blob.disabled_workflows, record.disabled_workflows);

        // A row written before the pause switch existed holds no `disabled_workflows`
        // key and loads as empty — the pre-#276 behaviour, no migration needed.
        let legacy = r#"{"agents":[],"desk_members":[]}"#;
        assert!(
            OverlayBlob::parse(legacy)
                .expect("pre-#276 object")
                .disabled_workflows
                .is_empty()
        );
        assert!(
            OverlayBlob::parse("[]")
                .expect("legacy array")
                .disabled_workflows
                .is_empty()
        );
    }

    /// A manifest with two teammates, one capped at $5/day and one uncapped —
    /// the two starting positions every budget-override case builds on.
    const BUDGET_ROSTER: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\nbudget_usd_daily = 5.0\n\
         [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n";

    fn budget_entry(agent_id: &str, cap: Option<f64>) -> BudgetOverride {
        BudgetOverride {
            agent_id: agent_id.to_string(),
            budget_usd_daily: cap,
            set_by: Actor {
                kind: ActorKind::User,
                id: "user-1".to_string(),
            },
            at_millis: 1_700_000_000_000,
        }
    }

    // ---- `[policy]` override (issue #562) --------------------------------

    const POLICY_MANIFEST: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\n\
         [policy]\nmode = \"supervised\"\n\
         always_approve = [\"payment.send\", \"filing.submit\"]\n";

    fn policy_entry(mode: Option<&str>, always: Option<Vec<&str>>) -> PolicyOverride {
        PolicyOverride {
            mode: mode.map(str::to_string),
            always_approve: always.map(|v| v.into_iter().map(str::to_string).collect()),
            set_by: Actor {
                kind: ActorKind::User,
                id: "user-1".to_string(),
            },
            at_millis: 1_700_000_000_000,
        }
    }

    /// With no override stored, `effective_policy` is the manifest verbatim —
    /// the pre-#562 behaviour, and the net that says adding this field changed
    /// nothing for a company that never uses it.
    #[test]
    fn effective_policy_falls_back_to_the_manifest() {
        let record = desk_record(POLICY_MANIFEST, Vec::new());
        let effective = record.effective_policy();
        assert_eq!(effective.mode, "supervised");
        assert_eq!(
            effective.always_approve,
            vec!["payment.send", "filing.submit"]
        );
    }

    /// A stored override beats the manifest. This is the "no redeploy" property
    /// at its source: nothing here consults `company.toml` once a row exists.
    #[test]
    fn a_stored_policy_override_beats_the_manifest() {
        let mut record = desk_record(POLICY_MANIFEST, Vec::new());
        record.overlay_policy = Some(policy_entry(Some("full"), None));
        assert_eq!(record.effective_policy().mode, "full");
    }

    /// Version skew can leave an override written by a newer host on a build
    /// that does not recognise its tier. Falling through to `supervised` would
    /// loosen a `readonly` seed, so the manifest wins for that field while any
    /// independently valid always-ask override remains in force.
    #[test]
    fn an_unknown_stored_policy_mode_cannot_loosen_the_manifest() {
        let manifest = POLICY_MANIFEST.replace("mode = \"supervised\"", "mode = \"readonly\"");
        let mut record = desk_record(&manifest, Vec::new());
        record.overlay_policy = Some(policy_entry(
            Some("future-tier"),
            Some(vec!["external.publish"]),
        ));

        let effective = record.effective_policy();
        assert_eq!(effective.mode, "readonly");
        assert_eq!(effective.always_approve, vec!["external.publish"]);
    }

    /// The two fields are independent: moving the tier must not silently reset
    /// the always-ask list to the manifest's, nor the reverse.
    ///
    /// This is the merge that makes the console usable — the tier control and
    /// the always-ask editor are separate widgets, and each `PUT` names only
    /// what it changed. If either field reset the other, using one control would
    /// quietly undo the other, and the always-ask list is the operator's real
    /// lever: it wins over every tier including `full`.
    #[test]
    fn overriding_one_policy_field_leaves_the_other_alone() {
        let mut record = desk_record(POLICY_MANIFEST, Vec::new());

        record.overlay_policy = Some(policy_entry(Some("full"), None));
        let effective = record.effective_policy();
        assert_eq!(effective.mode, "full");
        assert_eq!(
            effective.always_approve,
            vec!["payment.send", "filing.submit"],
            "moving the tier must not discard the manifest's always-ask list"
        );

        record.overlay_policy = Some(policy_entry(None, Some(vec!["external.publish"])));
        let effective = record.effective_policy();
        assert_eq!(
            effective.mode, "supervised",
            "editing the always-ask list must not move the tier"
        );
        assert_eq!(effective.always_approve, vec!["external.publish"]);
    }

    /// An emptied always-ask list is a real state, not a fallback.
    ///
    /// `Some(vec![])` is an operator deliberately clearing the list; `None` is
    /// "not overridden". If these collapsed, an operator clearing the list would
    /// instead get the manifest's three defaults back — silently re-imposing the
    /// gates they had just removed, and with no way to express what they meant.
    #[test]
    fn an_emptied_always_approve_list_is_not_a_fallback() {
        let mut record = desk_record(POLICY_MANIFEST, Vec::new());

        record.overlay_policy = Some(policy_entry(None, Some(vec![])));
        assert!(
            record.effective_policy().always_approve.is_empty(),
            "an explicitly emptied always-ask list must survive as empty"
        );

        record.overlay_policy = Some(policy_entry(None, None));
        assert_eq!(
            record.effective_policy().always_approve,
            vec!["payment.send", "filing.submit"],
            "an absent field must fall through to the manifest"
        );
    }

    /// `auto_approve_under_usd` is not overridable and keeps reading the
    /// manifest, so the merge cannot silently drop a threshold it does not carry.
    #[test]
    fn the_spend_threshold_is_untouched_by_a_policy_override() {
        let manifest = "[company]\nname = \"Acme\"\n\
             [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\n\
             [policy]\nmode = \"supervised\"\nauto_approve_under_usd = 2.5\n";
        let mut record = desk_record(manifest, Vec::new());
        record.overlay_policy = Some(policy_entry(Some("full"), Some(vec![])));
        assert_eq!(record.effective_policy().auto_approve_under_usd, Some(2.5));
    }

    /// Issue #343: with no override stored, `effective_budget` is the manifest
    /// value verbatim — the pre-#343 behaviour, and the regression net that says
    /// adding this field changed nothing for a company that never uses it.
    #[test]
    fn effective_budget_falls_back_to_the_manifest() {
        let record = desk_record(BUDGET_ROSTER, Vec::new());
        assert_eq!(record.effective_budget("analyst"), Some(5.0));
        assert_eq!(record.effective_budget("writer"), None);
        // An id on no roster at all is uncapped rather than an error: the gate
        // reads this per dispatched agent and must not invent a cap.
        assert_eq!(record.effective_budget("nobody"), None);
    }

    /// A stored override wins over the manifest in both directions — raising a
    /// cap and lowering one. This is the "no redeploy" property at its source:
    /// nothing here consults `company.toml` once a row exists.
    #[test]
    fn a_stored_override_beats_the_manifest() {
        let mut record = desk_record(BUDGET_ROSTER, Vec::new());
        record
            .overlay_budgets
            .push(budget_entry("analyst", Some(50.0)));
        assert_eq!(record.effective_budget("analyst"), Some(50.0));

        record.overlay_budgets = vec![budget_entry("analyst", Some(1.0))];
        assert_eq!(record.effective_budget("analyst"), Some(1.0));
    }

    /// The distinction the issue calls out by name: clearing a cap and setting
    /// it to zero are different states and must not collapse into each other.
    ///
    /// `Some(0.0)` caps the teammate at nothing (it will refuse to dispatch);
    /// `None` means explicitly uncapped and beats the manifest's $5. If these
    /// two ever resolved the same way, an operator lifting a cap would instead
    /// have silenced the teammate completely — the opposite of what they asked
    /// for, and unrecoverable from the console.
    #[test]
    fn clearing_a_cap_is_not_the_same_as_zeroing_it() {
        let mut record = desk_record(BUDGET_ROSTER, Vec::new());

        record.overlay_budgets = vec![budget_entry("analyst", Some(0.0))];
        assert_eq!(record.effective_budget("analyst"), Some(0.0));

        record.overlay_budgets = vec![budget_entry("analyst", None)];
        assert_eq!(
            record.effective_budget("analyst"),
            None,
            "an explicitly-uncapped override must beat the manifest's cap"
        );
    }

    /// An **overlay** teammate has no manifest row, so before #343 it could not
    /// be capped at all. A stored override caps it like anyone else — and
    /// dropping that override returns it to uncapped, since there is no manifest
    /// value underneath to fall back to.
    #[test]
    fn an_overlay_teammate_can_be_capped() {
        let mut record = desk_record(BUDGET_ROSTER, Vec::new());
        record.overlay_agents.push(OverlayAgent {
            id: "shane".to_string(),
            name: "Shane".to_string(),
            role: "Growth".to_string(),
            description: None,
            tools: Vec::new(),
        });
        assert_eq!(record.effective_budget("shane"), None);

        record.overlay_budgets = vec![budget_entry("shane", Some(2.5))];
        assert_eq!(record.effective_budget("shane"), Some(2.5));

        record.overlay_budgets.clear();
        assert_eq!(record.effective_budget("shane"), None);
    }

    /// Issue #343: one override per teammate. `upsert_budget_override` replaces
    /// the held row instead of appending a second, so the cap an admin last set
    /// is the cap every surface reads.
    ///
    /// Appending would leave the *first* row winning `budget_override`'s
    /// find-first read — meaning a raise or a revocation would persist happily
    /// and change nothing, the failure mode hardest to notice from the console.
    #[test]
    fn upserting_an_override_replaces_rather_than_appends() {
        let mut record = desk_record(BUDGET_ROSTER, Vec::new());
        record.upsert_budget_override(budget_entry("analyst", Some(50.0)));
        record.upsert_budget_override(budget_entry("writer", Some(3.0)));
        record.upsert_budget_override(budget_entry("analyst", None));

        assert_eq!(
            record.overlay_budgets.len(),
            2,
            "a second write for one teammate must replace, not accumulate: {:?}",
            record.overlay_budgets
        );
        assert_eq!(
            record.effective_budget("analyst"),
            None,
            "the latest write must win over the manifest's $5"
        );
        assert_eq!(record.effective_budget("writer"), Some(3.0));
    }

    /// Issue #343: duplicates are detectable, so a caller holding overrides it
    /// did not write (a bundle import) can refuse them instead of silently
    /// applying whichever row happens to sort first.
    #[test]
    fn duplicate_overrides_are_detected() {
        let mut record = desk_record(BUDGET_ROSTER, Vec::new());
        assert_eq!(record.duplicate_budget_agent_id(), None);

        record.overlay_budgets = vec![
            budget_entry("analyst", Some(9.0)),
            budget_entry("writer", None),
        ];
        assert_eq!(
            record.duplicate_budget_agent_id(),
            None,
            "distinct teammates are not a duplicate"
        );

        // Two rows for one teammate that disagree about the cap — the case where
        // guessing would either over-restrict or hand back a revoked allowance.
        record.overlay_budgets = vec![
            budget_entry("analyst", Some(9.0)),
            budget_entry("writer", None),
            budget_entry("analyst", Some(0.0)),
        ];
        assert_eq!(record.duplicate_budget_agent_id(), Some("analyst"));
    }

    /// Issue #343: the budget overrides round-trip through the `OverlayBlob` the
    /// sqlite/mongodb stores persist, and pre-#343 rows load as "no overrides"
    /// (the manifest still decides) rather than failing to parse.
    #[test]
    fn overlay_blob_round_trips_budgets() {
        let mut record = desk_record(BUDGET_ROSTER, Vec::new());
        record.overlay_budgets = vec![
            budget_entry("analyst", Some(9.0)),
            budget_entry("writer", None),
        ];
        let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
        let blob = OverlayBlob::parse(&json).expect("reparse");
        assert_eq!(blob.budgets, record.overlay_budgets);

        let legacy = r#"{"agents":[],"desk_members":[]}"#;
        assert!(
            OverlayBlob::parse(legacy)
                .expect("pre-budget object")
                .budgets
                .is_empty()
        );
        assert!(
            OverlayBlob::parse("[]")
                .expect("legacy array")
                .budgets
                .is_empty()
        );
    }

    /// An operator-created overlay desk resolves through the same
    /// `effective_desk_members` / `resolve_desk_id` / `desk_exists` helpers the
    /// manifest desks use, so the REST list and the harness desk-lead resolver
    /// treat it identically. Member additions still layer on top.
    #[test]
    fn overlay_desk_resolves_like_a_manifest_desk() {
        let manifest = "[company]\nname = \"Acme\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n";
        let mut record = desk_record(manifest, Vec::new());
        record.overlay_desks.push(OverlayDesk {
            id: "growth".into(),
            name: "Growth".into(),
            description: None,
            members: vec!["eng".into()],
        });
        // Resolves by id and by case-insensitive name.
        assert_eq!(record.resolve_desk_id("growth").as_deref(), Some("growth"));
        assert_eq!(record.resolve_desk_id("GROWTH").as_deref(), Some("growth"));
        assert!(record.desk_exists("growth"));
        // Founding member is the lead; a later overlay addition appends.
        assert_eq!(
            record.effective_desk_members("growth"),
            vec!["eng".to_string()]
        );
        record.overlay_desk_members.push(OverlayDeskMember {
            desk_id: "growth".into(),
            agent_id: "ceo".into(),
        });
        assert_eq!(
            record.effective_desk_members("growth"),
            vec!["eng".to_string(), "ceo".to_string()]
        );
    }

    /// The overlay blob round-trips operator-created desks through its persisted
    /// JSON form, so a created desk survives a store save/load cycle.
    #[test]
    fn overlay_blob_round_trips_desks() {
        let with_desks = r#"{"agents":[],"desk_members":[],"desks":[{"id":"growth","name":"Growth","members":["eng"]}]}"#;
        let blob = OverlayBlob::parse(with_desks).expect("object with desks");
        assert_eq!(blob.desks.len(), 1);
        assert_eq!(blob.desks[0].id, "growth");
        assert_eq!(blob.desks[0].members, vec!["eng".to_string()]);
        // Re-serialize and re-parse — the desk survives the round trip.
        let json = serde_json::to_string(&blob).expect("serialize");
        let again = OverlayBlob::parse(&json).expect("reparse");
        assert_eq!(again.desks, blob.desks);
    }

    // ── Issue #228: a workflow run's outcome is journaled ───────────────────

    fn delivery(node: &str, status: DeliveryStatus) -> DeliveryReport {
        DeliveryReport {
            node: node.to_string(),
            kind: "owner".to_string(),
            target: Some("ada@example.com".to_string()),
            status,
            detail: "emailed the company's admin".to_string(),
            reason: crate::ports::DeliveryReason::OwnerEmailed,
        }
    }

    /// The full-bodied variant survives the JSONL round trip the journal puts
    /// every event through — including the delivery rows, which are the whole
    /// reason the event exists.
    #[test]
    fn workflow_run_finished_round_trips_with_every_field() {
        let event = CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: true,
            run_id: Some("run-1".to_string()),
            deliveries: vec![
                delivery("owner_summary", DeliveryStatus::Skipped),
                delivery("also_sent", DeliveryStatus::Sent),
            ],
            pending_approvals: vec!["review".to_string()],
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };
        assert_eq!(round_trip(&event), event);
    }

    /// The failed-run shape round-trips too. This is the arm that today only
    /// warns to host stdout, so it is the one an operator most needs read back.
    #[test]
    fn workflow_run_finished_round_trips_a_failed_run() {
        let event = CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: true,
            run_id: None,
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: Some("agent node `worker` had no inference source".to_string()),
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };
        assert_eq!(round_trip(&event), event);
    }

    /// The additive contract, both halves.
    ///
    /// **Forward:** a minimal line — only the two required fields, exactly what
    /// a future/older writer might emit — still loads, so no persisted journal
    /// needs migrating.
    ///
    /// **Backward:** an empty run serializes to *only* those two fields. Every
    /// optional/collection field is `skip_serializing_if`, which is what keeps
    /// the wire form of an outcome-less run minimal rather than littered with
    /// nulls and `[]`s.
    #[test]
    fn workflow_run_finished_omits_and_defaults_its_optional_fields() {
        let json = r#"{"kind":"WorkflowRunFinished","workflow_id":"digest","scheduled":false}"#;
        let event: CompanyEvent = serde_json::from_str(json).expect("minimal line loads");
        assert_eq!(
            event,
            CompanyEvent::WorkflowRunFinished {
                workflow_id: "digest".to_string(),
                scheduled: false,
                run_id: None,
                deliveries: Vec::new(),
                pending_approvals: Vec::new(),
                error: None,
                cancelled: false,
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: Vec::new(),
                approvals: Vec::new(),
            }
        );
        // …and serializing it back emits nothing extra.
        let out = serde_json::to_string(&event).expect("serialize");
        assert!(!out.contains("run_id"), "{out}");
        assert!(!out.contains("deliveries"), "{out}");
        assert!(!out.contains("pending_approvals"), "{out}");
        assert!(!out.contains("error"), "{out}");
        // Issue #383's field joins the same contract, which is what makes it
        // replay-safe: absent decodes as `false`, and a non-cancelled run's line
        // is byte-identical to what it was before the field existed.
        assert!(!out.contains("cancelled"), "{out}");
    }

    /// Issue #661 (M5): a run's board rows round-trip, and a line written before
    /// they existed still replays.
    ///
    /// Three claims, and the last two are what make this additive rather than a
    /// migration: the rows survive the round trip in camelCase; a run that touched
    /// no card serializes with **no `board` key at all**, so every already-written
    /// journal line stays byte-identical; and a pre-#661 line decodes as empty
    /// rather than failing to decode.
    #[test]
    fn workflow_run_finished_round_trips_board_rows() {
        use crate::ports::workflow_runner::WorkflowBoardAction;

        let event = CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: true,
            run_id: Some("run-1".to_string()),
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: vec![WorkflowRunBoardRow {
                action: WorkflowBoardAction::Spawned,
                task_id: Some("card-1".to_string()),
                title: Some("Reply to the auditor".to_string()),
                assignee: None,
            }],
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };
        assert_eq!(round_trip(&event), event);
        let out = serde_json::to_string(&event).expect("serialize");
        assert!(out.contains("\"action\":\"spawned\""), "{out}");
        assert!(out.contains("\"taskId\":\"card-1\""), "{out}");
        // Absent rather than null on the arm that has nothing to say.
        assert!(!out.contains("assignee"), "{out}");

        // A run that touched no card is byte-unchanged from pre-#661.
        let untouched = CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: true,
            run_id: Some("run-1".to_string()),
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };
        let out = serde_json::to_string(&untouched).expect("serialize");
        assert!(!out.contains("board"), "{out}");

        // And a line written before the field existed replays as empty.
        let legacy = serde_json::json!({
            "kind": "WorkflowRunFinished",
            "workflow_id": "digest",
            "scheduled": true,
            "run_id": "run-1"
        });
        let loaded: CompanyEvent =
            serde_json::from_value(legacy).expect("a pre-#661 journal line replays");
        let CompanyEvent::WorkflowRunFinished { board, .. } = loaded else {
            panic!("expected a WorkflowRunFinished");
        };
        assert!(board.is_empty());
    }

    /// Issue #383: a cancelled run round-trips, and is distinguishable from a
    /// failed one by more than the absence of an error.
    ///
    /// The pairing is the assertion. A cancelled run carries `cancelled: true`
    /// **and** `error: None` — so a reader that only ever looked at `error`
    /// (every reader before #383) sees a clean finish, which is exactly why the
    /// console needed a new field rather than a new error string.
    #[test]
    fn workflow_run_finished_round_trips_a_cancelled_run() {
        let event = CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: false,
            run_id: Some("run-1".to_string()),
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: true,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };
        assert_eq!(round_trip(&event), event);

        let out = serde_json::to_string(&event).expect("serialize");
        assert_eq!(
            out,
            r#"{"kind":"WorkflowRunFinished","workflow_id":"digest","scheduled":false,"run_id":"run-1","cancelled":true}"#,
            "the cancelled line pins its exact wire shape"
        );
    }

    /// A pre-#383 line — the overwhelming majority of every journal on disk —
    /// loads as not cancelled rather than failing to decode.
    #[test]
    fn a_pre_383_finished_line_loads_as_not_cancelled() {
        let line = r#"{"kind":"WorkflowRunFinished","workflow_id":"digest","scheduled":true,"run_id":"run-9","error":"it broke"}"#;
        let event: CompanyEvent = serde_json::from_str(line).expect("pre-#383 line loads");
        let CompanyEvent::WorkflowRunFinished {
            cancelled, error, ..
        } = &event
        else {
            panic!("expected a WorkflowRunFinished");
        };
        assert!(!cancelled, "an old failed run must not read as cancelled");
        assert_eq!(error.as_deref(), Some("it broke"));
        // And re-serializing it stays byte-identical — the field is absent
        // going out as well as coming in.
        assert_eq!(
            serde_json::to_string(&event).expect("serialize"),
            line,
            "re-writing an old line must not add the new field"
        );
    }

    /// Issue #371's opening bracket round-trips through the JSONL the journal
    /// puts every event through.
    #[test]
    fn workflow_run_started_round_trips() {
        let event = CompanyEvent::WorkflowRunStarted {
            workflow_id: "digest".to_string(),
            run_id: "run-1".to_string(),
            scheduled: true,
        };
        assert_eq!(round_trip(&event), event);
    }

    /// Both node outcomes round-trip, including the elapsed reading — the field
    /// that turns "it finished" into "it took this long", which is what tells a
    /// slow run from a wedged one.
    #[test]
    fn workflow_node_finished_round_trips_both_statuses() {
        for status in [
            WorkflowNodeStatus::Ok,
            WorkflowNodeStatus::Error,
            // Issue #881's third arm. Pinned in the same loop rather than a
            // test of its own so a fourth reading cannot be added without
            // someone editing this list.
            WorkflowNodeStatus::Blocked,
        ] {
            let event = CompanyEvent::WorkflowNodeFinished {
                workflow_id: "digest".to_string(),
                run_id: "run-1".to_string(),
                node_id: "ceo".to_string(),
                status,
                elapsed_ms: 1234,
                diagnostics: Vec::new(),
            };
            assert_eq!(round_trip(&event), event);
        }
    }

    /// A `WorkflowRunFinished` line written before #881 / #880 still replays.
    ///
    /// **This is not a nicety.** The event is folded at boot, so a new field
    /// without `#[serde(default)]` would make every pre-existing journal line
    /// fail to parse — and the failure mode is a company silently losing its
    /// whole run history, not a compile error. Pinned against a hand-written
    /// legacy payload rather than a round-trip, because a round-trip can only
    /// ever prove the new shape agrees with itself.
    #[test]
    fn a_pre_881_run_finished_line_still_replays() {
        let legacy = serde_json::json!({
            "kind": "WorkflowRunFinished",
            "workflow_id": "digest",
            "scheduled": true,
            "run_id": "run-1",
            "pending_approvals": ["review"],
            "cancelled": false
        });
        let event: CompanyEvent =
            serde_json::from_value(legacy).expect("a pre-#881 journal line must still parse");
        let CompanyEvent::WorkflowRunFinished {
            blocked_nodes,
            approvals,
            pending_approvals,
            ..
        } = &event
        else {
            panic!("expected a WorkflowRunFinished, got {event:?}");
        };
        assert!(blocked_nodes.is_empty());
        assert!(approvals.is_empty());
        assert_eq!(pending_approvals, &vec!["review".to_string()]);
    }

    /// A run that blocked on nobody serializes byte-for-byte as it did before
    /// #881 / #880 — which is nearly every run, so this is what keeps the
    /// journal from growing two empty arrays per line.
    #[test]
    fn a_run_that_blocked_on_nobody_adds_no_keys() {
        let event = CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: false,
            run_id: Some("run-1".to_string()),
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert!(json.get("blocked_nodes").is_none(), "{json}");
        assert!(json.get("approvals").is_none(), "{json}");
    }

    /// Every field on both #371 variants is required, and that is the point:
    /// the correlation id is what groups a run's nodes with its outcome, so a
    /// line without one would be unfoldable. Nothing is `skip_serializing_if`,
    /// so the wire form is fully self-describing.
    #[test]
    fn workflow_progress_variants_serialize_every_field() {
        let started = serde_json::to_string(&CompanyEvent::WorkflowRunStarted {
            workflow_id: "digest".to_string(),
            run_id: "run-1".to_string(),
            scheduled: false,
        })
        .expect("serialize");
        assert_eq!(
            started,
            r#"{"kind":"WorkflowRunStarted","workflow_id":"digest","run_id":"run-1","scheduled":false}"#
        );

        let node = serde_json::to_string(&CompanyEvent::WorkflowNodeFinished {
            workflow_id: "digest".to_string(),
            run_id: "run-1".to_string(),
            node_id: "ceo".to_string(),
            status: WorkflowNodeStatus::Error,
            elapsed_ms: 7,
            diagnostics: Vec::new(),
        })
        .expect("serialize");
        assert_eq!(
            node,
            r#"{"kind":"WorkflowNodeFinished","workflow_id":"digest","run_id":"run-1","node_id":"ceo","status":"error","elapsed_ms":7}"#
        );
    }

    /// The replay guarantee #371 rests on, stated as a test: adding these two
    /// variants cannot change how an already-persisted line loads. A journal
    /// written before #371 contains neither `kind`, and the pre-#371 wire form
    /// of the variant they sit beside still decodes byte-for-byte as it did.
    #[test]
    fn pre_371_journal_lines_are_unaffected_by_the_new_variants() {
        let line = r#"{"kind":"WorkflowRunFinished","workflow_id":"digest","scheduled":true,"pending_approvals":["review"]}"#;
        let event: CompanyEvent = serde_json::from_str(line).expect("pre-#371 line loads");
        assert_eq!(
            event,
            CompanyEvent::WorkflowRunFinished {
                workflow_id: "digest".to_string(),
                scheduled: true,
                run_id: None,
                deliveries: Vec::new(),
                pending_approvals: vec!["review".to_string()],
                error: None,
                cancelled: false,
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: Vec::new(),
                approvals: Vec::new(),
            }
        );
    }

    /// Issue #327: the workspace announcement survives the JSONL round trip the
    /// journal puts every event through, and carries its discriminant.
    #[test]
    fn workspace_changed_round_trips() {
        let event = CompanyEvent::WorkspaceChanged {
            node_id: "n-1".to_string(),
            change: "updated".to_string(),
        };
        assert_eq!(round_trip(&event), event);
        assert_eq!(event.kind(), "WorkspaceChanged");
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains(r#""kind":"WorkspaceChanged""#), "{json}");
        assert!(json.contains(r#""node_id":"n-1""#), "{json}");
    }

    /// The one variant whose retention class diverges from its sibling's, so
    /// the choice is pinned rather than left to the next reader's memory.
    ///
    /// `WorkspaceChanged` is Prunable: it is high-volume machine exhaust whose
    /// whole meaning is "re-read the tree", nothing addresses it by sequence,
    /// and nothing folds it at boot. `TaskCardChanged` stays Permanent because
    /// a board card's lifecycle is the company's work history.
    #[test]
    fn a_workspace_announcement_is_prunable_though_its_board_sibling_is_not() {
        use crate::ports::events::RetentionClass;

        assert_eq!(
            CompanyEvent::WorkspaceChanged {
                node_id: "n-1".to_string(),
                change: "updated".to_string(),
            }
            .retention_class(),
            RetentionClass::Prunable
        );
        assert_eq!(
            CompanyEvent::TaskCardChanged {
                task_id: "t-1".to_string(),
                change: "opened".to_string(),
                column: Some("todo".to_string()),
            }
            .retention_class(),
            RetentionClass::Permanent
        );
    }

    /// Issue #529: the delivered-report event survives the JSONL round trip the
    /// journal puts every event through, `target` and all — the whole reason it
    /// exists is to be read back after a crash.
    #[test]
    fn workflow_report_delivered_round_trips() {
        let event = CompanyEvent::WorkflowReportDelivered {
            workflow_id: "digest".to_string(),
            run_id: "run-1".to_string(),
            node: "owner_summary".to_string(),
            kind: "owner".to_string(),
            target: Some("ada@example.com".to_string()),
        };
        assert_eq!(round_trip(&event), event);
    }

    /// Issue #529: the wire shape is pinned, and `target` is omitted entirely
    /// when a destination named none — the same `skip_serializing_if` economy
    /// every optional field on this enum keeps, so a channel line stays minimal.
    #[test]
    fn workflow_report_delivered_pins_its_wire_shape_and_omits_absent_target() {
        let with_target = serde_json::to_string(&CompanyEvent::WorkflowReportDelivered {
            workflow_id: "digest".to_string(),
            run_id: "run-1".to_string(),
            node: "owner_summary".to_string(),
            kind: "owner".to_string(),
            target: Some("ada@example.com".to_string()),
        })
        .expect("serialize");
        assert_eq!(
            with_target,
            r#"{"kind":"WorkflowReportDelivered","workflow_id":"digest","run_id":"run-1","node":"owner_summary","destination_kind":"owner","target":"ada@example.com"}"#
        );

        let no_target = serde_json::to_string(&CompanyEvent::WorkflowReportDelivered {
            workflow_id: "digest".to_string(),
            run_id: "run-1".to_string(),
            node: "notice".to_string(),
            kind: "channel".to_string(),
            target: None,
        })
        .expect("serialize");
        assert_eq!(
            no_target,
            r#"{"kind":"WorkflowReportDelivered","workflow_id":"digest","run_id":"run-1","node":"notice","destination_kind":"channel"}"#,
            "an absent target must not ride the line as a null"
        );
        // …and a line with no `target` loads back as `None` rather than failing.
        let decoded: CompanyEvent = serde_json::from_str(&no_target).expect("minimal line loads");
        assert_eq!(
            decoded,
            CompanyEvent::WorkflowReportDelivered {
                workflow_id: "digest".to_string(),
                run_id: "run-1".to_string(),
                node: "notice".to_string(),
                kind: "channel".to_string(),
                target: None,
            }
        );
    }

    /// Issue #259's two variants pin their wire shape the same way
    /// `WorkflowCreated` does: `kind` + `workflow_id` + `name`, with `by`
    /// omitted entirely when absent so the common unattributed line stays the
    /// short one.
    #[test]
    fn workflow_updated_and_deleted_pin_their_wire_shape() {
        let updated = CompanyEvent::WorkflowUpdated {
            workflow_id: "digest".to_string(),
            name: "Daily digest".to_string(),
            by: None,
        };
        assert_eq!(
            serde_json::to_string(&updated).expect("serialize"),
            r#"{"kind":"WorkflowUpdated","workflow_id":"digest","name":"Daily digest"}"#
        );

        let deleted = CompanyEvent::WorkflowDeleted {
            workflow_id: "digest".to_string(),
            name: "Daily digest".to_string(),
            by: None,
        };
        assert_eq!(
            serde_json::to_string(&deleted).expect("serialize"),
            r#"{"kind":"WorkflowDeleted","workflow_id":"digest","name":"Daily digest"}"#
        );

        // Both round-trip.
        for event in [updated, deleted] {
            let line = serde_json::to_string(&event).expect("serialize");
            let back: CompanyEvent = serde_json::from_str(&line).expect("deserialize");
            assert_eq!(back, event);
        }
    }

    /// The graph body must never reach the journal — see the variant docs. A
    /// reader of the shared append-only log (operator SSE, the inference
    /// sidecar) has no business seeing agent prompts or destination addresses,
    /// and the only way a body could leak here is someone adding a field.
    #[test]
    fn workflow_updated_carries_no_graph_body() {
        let line = serde_json::to_string(&CompanyEvent::WorkflowUpdated {
            workflow_id: "digest".to_string(),
            name: "Daily digest".to_string(),
            by: None,
        })
        .expect("serialize");
        assert!(!line.contains("toml"), "{line}");
        assert!(!line.contains("node"), "{line}");
        assert!(!line.contains("graph"), "{line}");
    }

    /// **The backcompat proof.** A journal written before this variant existed
    /// still loads, line for line, and every one of those lines re-serializes
    /// byte-identically — which is what "additive, no migration" actually
    /// claims. Adding an enum variant cannot change how a sibling serializes,
    /// but nothing else in the suite asserts it for the whole log, and a
    /// regression here would corrupt an export/import round trip silently.
    #[test]
    fn a_journal_written_before_this_variant_still_loads_byte_identically() {
        // Verbatim lines in the pre-#228 on-disk shapes, including the pre-`by`
        // / pre-`chat` `OperatorMessage` and the pre-`steps` `AgentReply`.
        let legacy = [
            r#"{"kind":"OperatorMessage","text":"ship it"}"#,
            r#"{"kind":"AgentReply","chat_id":"general","agent_id":"ceo","text":"on it"}"#,
            r#"{"kind":"ScheduleFired","cron":"0 9 * * *","prompt":"daily"}"#,
            r#"{"kind":"WorkflowCreated","workflow_id":"digest","name":"Digest"}"#,
            r#"{"kind":"TaskDispatched","task_id":"t-1"}"#,
            r#"{"kind":"DeskTaskCompleted","task_id":"t-1","desk":"ceo","output":"done","column":"in_review"}"#,
        ];
        for line in legacy {
            let event: CompanyEvent = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("pre-#228 journal line must still load: {line} — {e}"));
            let again = serde_json::to_string(&event).expect("serialize");
            assert_eq!(again, line, "pre-#228 line must re-serialize unchanged");
        }
    }

    /// Issue #242: an effect remembers which task attempt produced it, and the
    /// field is additive in the same way `Effect::agent` was — a journal line
    /// written before it existed replays as `None` (no run correlation, the
    /// pre-#242 behaviour) rather than failing to parse and taking the whole
    /// approval queue down with it on replay.
    #[test]
    fn effect_run_id_round_trips_and_a_legacy_line_replays_as_none() {
        let mut effect = Effect {
            kind: "composio.execute".to_string(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" }),
            agent: Some("finance".to_string()),
            run_id: None,
        };
        let untagged = serde_json::to_string(&effect).expect("serialize");
        assert!(
            !untagged.contains("run_id"),
            "an untagged effect's wire form must be unchanged: {untagged}"
        );

        effect.run_id = Some("run-7".to_string());
        let tagged = serde_json::to_string(&effect).expect("serialize");
        assert!(tagged.contains(r#""run_id":"run-7""#), "{tagged}");
        assert_eq!(
            effect,
            serde_json::from_str::<Effect>(&tagged).expect("round trip")
        );

        // The pre-#242 line: same bytes, no field.
        let legacy: Effect = serde_json::from_str(&untagged).expect("legacy effect must load");
        assert_eq!(legacy.run_id, None);
        assert_eq!(
            legacy.agent.as_deref(),
            Some("finance"),
            "the earlier additive field must still be read alongside the new one"
        );
    }

    /// Issue #242: the run id rides the dispatch event, and it is additive in
    /// both directions — a tagged dispatch round-trips it, and an untagged one
    /// serializes exactly the shape a pre-#242 journal holds (asserted verbatim
    /// above too, but here against the *writer* rather than the reader).
    #[test]
    fn task_dispatched_carries_its_run_id_without_changing_the_untagged_shape() {
        let untagged = CompanyEvent::TaskDispatched {
            task_id: "t-1".to_string(),
            run_id: None,
        };
        assert_eq!(
            serde_json::to_string(&untagged).expect("serialize"),
            r#"{"kind":"TaskDispatched","task_id":"t-1"}"#
        );

        let tagged = CompanyEvent::TaskDispatched {
            task_id: "t-1".to_string(),
            run_id: Some("run-7".to_string()),
        };
        let line = serde_json::to_string(&tagged).expect("serialize");
        assert!(line.contains(r#""run_id":"run-7""#), "{line}");
        assert_eq!(
            tagged,
            serde_json::from_str::<CompanyEvent>(&line).expect("round trip")
        );

        // A legacy line loads as an untagged dispatch rather than failing.
        let legacy: CompanyEvent =
            serde_json::from_str(r#"{"kind":"TaskDispatched","task_id":"t-1"}"#).expect("legacy");
        assert_eq!(legacy, untagged);
    }

    #[test]
    fn legacy_agent_card_json_deserializes_with_defaults() {
        // A card written by an earlier phase carried only three fields; the new
        // `#[serde(default)]` fields must fill in without error.
        let json = r#"{"handle":"acme","description":"d","skills":["a"]}"#;
        let card: AgentCard = serde_json::from_str(json).expect("deserialize legacy card");
        assert_eq!(card.handle, "acme");
        assert!(card.name.is_empty());
        assert!(card.payment_requirements.is_empty());
        assert!(card.supported_interfaces.is_empty());
    }
}
