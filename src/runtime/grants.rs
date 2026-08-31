//! Single-use grants: what an operator's "approve" actually buys for a tool
//! call an agent was blocked from making (issue #243).
//!
//! ## Why a grant, and not just "run the effect"
//!
//! Two different things park on the same approval queue, and they need opposite
//! treatment on approval:
//!
//! * A **native** effect — an email the runtime sends, a workflow delivery, a
//!   Medulla effect frame. The runtime built it and the runtime can perform it,
//!   so approving it means executing it, at most once, keyed by approval id.
//! * A **harness tool call** — `composio_execute`, `workspace_write`,
//!   `media_generate_image`. openhuman's `ToolPolicy` is fail-closed: it blocked
//!   the call inside the agent's turn and fed the model a refusal. The
//!   opencompany [`Effect`](crate::ports::types::Effect) projected from it is a
//!   *description* of a tool call, not something the runtime knows how to
//!   perform — its payload is the tool's arguments. Executing it would ledger a
//!   spend and route a `{channel, text}` payload if one happened to be shaped
//!   like that, and otherwise do nothing at all. The real work is the tool, and
//!   only that agent can run it.
//!
//! So approving a harness call mints a **grant**: a one-shot permission slip
//! that lets exactly one future call — same agent, same tool, byte-identical
//! arguments — through the policy that blocked it, after which it is gone. The
//! agent is then re-dispatched with an instruction to re-issue the call.
//!
//! ## Why every part of that sentence is load-bearing
//!
//! * **Single-use.** Consuming removes the grant under the same lock that
//!   matched it, so a model that re-tries the tool in a loop gets exactly one
//!   execution out of one approval. Approving once must not mean "this tool is
//!   open now".
//! * **Agent-scoped.** A grant minted for `finance` does not let `marketing`
//!   through, even for the identical call. The operator approved a specific
//!   agent's request.
//! * **Exact arguments.** Matching is `serde_json::Value` equality on the whole
//!   argument object. A model that re-issues the call with a different
//!   recipient, a larger amount, or an extra field does not match, falls
//!   through, and re-parks — which is the honest outcome, because the operator
//!   never saw those arguments. Approve-with-edit is handled by minting against
//!   the *amended* arguments, so the operator's edit is what the grant admits.
//! * **TTL.** A grant the agent never redeems expires
//!   ([`GRANT_TTL_MILLIS`]) rather than sitting live forever. An approval is
//!   consent to an action now, not a standing authorisation; without this, a
//!   grant minted today would still fire if the same call surfaced next month.
//!
//! ## The second scope: a standing grant (issue #374)
//!
//! Single-use is the right *default* and was for a while the only mode, which
//! made it the whole design. An agent reaching for the same tool a dozen times
//! produced a dozen near-identical cards, and the operator's rational escapes
//! were approving blind or switching the company to `full` — throwing the gate
//! away to stop it nagging. So there is now a second scope the operator can
//! pick: [`StandingGrant`], "this tool, for this teammate, until a deadline".
//!
//! It is a **distinct type**, not a scope enum on [`GrantedCall`], and both
//! differences are load-bearing:
//!
//! * it has **no `args` field**, so it is structurally incapable of
//!   argument-matching or of being widened into one later;
//! * its expiry is **not optional**, so it is structurally incapable of living
//!   forever. The issue forbids silent accumulation, and a type that cannot
//!   express "no expiry" cannot regress into it.
//!
//! What it never covers is decided elsewhere, once:
//! [`Effect::may_be_granted_standing`](crate::ports::types::Effect::may_be_granted_standing)
//! applied to the parked effect, which asks what the tool can **reach** rather
//! than what its name is called (issue #444). Running an arbitrary command,
//! reaching an arbitrary address and overwriting operator-owned guidance are
//! all refused, as is every Spend / Send / Sign / Publish / Hire / Identity
//! consequence and every tool nobody has classified.
//!
//! Because it has no `args` field this type admits any arguments, which is a
//! fair summary of a tool's consequence only while consequence is a property of
//! the tool name. It is not one for `composio_execute`, so the policy
//! re-classifies the live call before honouring a grant — see
//! [`ApprovalPolicy::standing_grant_allows`](crate::harness::policy::ApprovalPolicy).
//!
//! That check keeps a send out of a read's grant, but it cannot tell one
//! provider's read from another's: they are both reads, under the same tool
//! name, for the same teammate. So the grant also records **which toolkit the
//! card was about** ([`StandingGrant::scope`], issue #457). The operator
//! consented to "read from GitHub"; without the scope the grant they got was
//! "make any Composio read, anywhere" — broader than the sentence they agreed
//! to, across every account the company had ever connected.
//!
//! ## Durability
//!
//! Both sets are in-memory, but their lifecycles are journaled
//! (`ApprovalGranted` / `GrantConsumed` / `GrantExpired` for single-use;
//! `StandingGrantMinted` / `StandingGrantRevoked` / `StandingGrantExpired` for
//! standing) and replayed on boot via
//! [`RuntimeJournal::replayed_grants`](crate::runtime::journal::RuntimeJournal::replayed_grants)
//! and its standing counterpart, so a restart between "operator approved" and
//! "agent re-issued" does not silently drop the approval. Consumed, revoked and
//! expired grants are folded *out* on replay, so a restart can never resurrect
//! one that already fired or one the operator took back.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;

use crate::ports::generate_id;
use crate::ports::types::{
    Actor, ApprovalId, Attachment, CompanyEvent, CompanyId, EventSeq, Mention, MessageIntent,
    Verdict,
};

/// How long an unredeemed grant stays live: 15 minutes.
///
/// Sized to the gap between an operator hitting approve and the granting agent
/// finishing its re-dispatched turn — generous for a model turn, far short of
/// "still valid tomorrow". Expiry is not silent: the sweep tells the operator
/// the agent did not act, so a re-approval is an informed choice rather than a
/// mystery.
pub const GRANT_TTL_MILLIS: u64 = 15 * 60 * 1000;

/// One approved-but-not-yet-redeemed tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantedCall {
    /// The approval the operator resolved to mint this grant.
    pub approval_id: ApprovalId,
    /// The roster agent allowed to redeem it. Nobody else matches.
    pub agent: String,
    /// The tool the grant admits — the parked effect's `kind`.
    pub tool: String,
    /// The exact arguments admitted. Matching is whole-value equality.
    pub args: serde_json::Value,
    /// Epoch-millis the grant was minted, for TTL expiry.
    pub at_millis: u64,
    /// The chat thread the approval was raised in (issue #379) — copied off the
    /// approval's origin when the grant is minted, so the re-dispatched turn's
    /// reply is journaled back into the conversation that asked for it.
    ///
    /// Deliberately **not** part of the redemption match: the operator approved
    /// a call, not a location, and a grant that failed to match because the
    /// turn came back on a different thread would silently re-park. It rides
    /// along purely as routing.
    ///
    /// `None` when the parked approval carried no thread (a workflow delivery,
    /// a scheduler tick) and on a grant replayed from a journal line written
    /// before this field existed. Both fall back to
    /// [`agent`](Self::agent) — today's behaviour, which is right for a DM and
    /// was never right for a desk channel.
    #[serde(default)]
    pub origin_thread: Option<String>,
    /// The thread *within* [`origin_thread`](Self::origin_thread) the approval
    /// was raised in (issue #435) — the root the raising message hangs off,
    /// copied off the approval's origin alongside the channel.
    ///
    /// Rides along on exactly the same terms as `origin_thread`, and for the
    /// same reason: **not** part of the redemption match. The operator approved
    /// a call, not a location, and a grant that failed to match because the
    /// turn came back on a different thread would silently re-park — the
    /// failure #379 called out, one axis finer. It is routing, and only
    /// routing.
    ///
    /// That is safe by construction rather than by care: [`GrantSet::consume`]
    /// matches field by field on `agent`, `tool` and `args`, so a field added
    /// here cannot join the predicate by accident.
    ///
    /// `None` when the approval was raised straight in a channel rather than
    /// inside a thread, and on a grant replayed from a line written before this
    /// field existed. Both mean the continuation answers in the channel, which
    /// is the pre-#435 behaviour.
    #[serde(default)]
    pub origin_parent: Option<EventSeq>,
    /// The task the parked call belonged to, when it was raised from a task turn
    /// (issue #796) — copied off the approval's `approval_task` join when the
    /// grant is minted.
    ///
    /// **Routing only, never part of the redemption match**, on exactly the same
    /// terms as [`origin_thread`](Self::origin_thread): the operator approved a
    /// call, not a task, and [`GrantSet::consume`] matches on `(agent, tool,
    /// args)` alone. It rides along so the re-dispatched turn can reclaim the
    /// task's held-across-park checkout ([`CheckoutLedger`] in
    /// `crate::harness::repo`) and so a denied or expired grant's tree can be
    /// swept once no live grant names the task.
    ///
    /// `None` for an approval raised outside a task (a plain operator chat) and
    /// on a grant replayed from a line written before this field existed — both
    /// mean there is no task checkout to resume, which is the pre-#796 behaviour.
    #[serde(default)]
    pub origin_task: Option<String>,
}

/// A durable follow-up owed after an agent explicitly asked the operator a
/// question. This is deliberately not a grant: either verdict resumes the
/// conversation, and a denial must never appear in the audit log as authority
/// to execute a tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalContinuation {
    /// Routing and agent context for the follow-up turn.
    pub call: GrantedCall,
    /// The operator decision the follow-up must report.
    pub verdict: Verdict,
    /// Who made the decision, retained so restart recovery can recreate the
    /// exact `ApprovalResolved` event without inventing an actor.
    pub by: Actor,
}

/// The hard ceiling on a standing grant's life: 7 days.
///
/// A request past this is a **400**, never a silent clamp. Quietly shortening a
/// duration the operator chose would leave them believing a permission is live
/// when it lapsed days earlier — the failure this issue exists to stop, in the
/// opposite direction.
pub const MAX_STANDING_GRANT_MILLIS: u64 = 7 * 24 * 60 * 60 * 1000;

/// A standing grant's own id.
///
/// Separate from [`ApprovalId`] because the two have different lifetimes: the
/// approval is resolved and gone, the grant it minted outlives it and is what
/// the operator later revokes. Keying revocation on the approval id would tie
/// the revoke route to a record that is, from the operator's side, history.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GrantId(String);

impl GrantId {
    /// Wraps an existing grant id string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Mints a fresh grant id.
    pub fn generate() -> Self {
        Self(generate_id())
    }
}

impl From<String> for GrantId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for GrantId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GrantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What an operator's approve actually buys (issue #374).
///
/// Two options, and only two. Count-based ("the next 5 calls") was rejected: it
/// reintroduces the annoyance nondeterministically and needs a durable decrement
/// on the hot path. "For this session" was rejected because the runtime has no
/// operator-session object that an agent's work spans — it would be a fiction.
/// "Forever" is what the issue forbids.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GrantScope {
    /// Today's behaviour, byte for byte: one call, argument-exact, agent-scoped.
    /// The default, and what every caller that says nothing gets.
    #[default]
    Once,
    /// This tool, for this teammate, with any arguments, until
    /// `expires_at_millis` — an absolute epoch-millis deadline.
    Tool {
        /// Absolute epoch-millis the grant stops admitting calls.
        expires_at_millis: u64,
    },
}

/// Who may spend a standing permission (issue #1098).
///
/// Two kinds, and the second is the whole of that issue. A scheduled workflow
/// re-asked the same question on every run because a permission could only name
/// a teammate, and a graph that is simply running has none — so the case whose
/// calls were *pre-declared by an operator* was the one case standing permission
/// could not cover, while an agent choosing its arguments at run time could hold
/// one.
///
/// # Why a workflow and not one of its nodes
///
/// The operator consented to a host, and a second call to that host from the
/// same job is the same proposition they already agreed to. Keying on the node
/// would re-park it — the workflow-shaped version of slug-exactness, which
/// [`StandingGrant::scope`] records was rejected for Composio because it "would
/// re-park every new action and make the grant worth nothing".
///
/// The cost is stated rather than hidden: for the six tools that are grantable
/// with no scope at all (`file_write`, `edit`, `apply_patch`, `csv_export`,
/// `memory_store`, `publish_artifact`) nothing narrows a workflow permission, so
/// a node added inside the window inherits it. Three things bound that, and are
/// why it is the accepted trade: `shell` and `http_request` are
/// [`Standing::PerCall`](crate::policy::Standing) and never persist at all, the
/// ceiling is seven days, and a permission is per-tool — "may write files" never
/// implies "may publish". Narrowing this later means adding a node id to the
/// workflow variant, and nothing else.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GrantSubject {
    /// A roster teammate. Every permission minted before issue #1098.
    Agent(String),
    /// One authored workflow, by the stable id its file declares — not the run
    /// id, which is fresh on every firing and would make each permission die
    /// with the run that minted it.
    Workflow(String),
}

impl GrantSubject {
    /// A teammate subject from anything string-like.
    pub fn agent(id: impl Into<String>) -> Self {
        Self::Agent(id.into())
    }

    /// A workflow subject from anything string-like.
    pub fn workflow(id: impl Into<String>) -> Self {
        Self::Workflow(id.into())
    }
}

/// Who could hold a standing permission for `effect`, or `None` when nobody
/// could (issue #1098).
///
/// **The one place this is decided.** Three surfaces ask it — the card's
/// `broadly_grantable` flag, the resolve route's 400, and the mint — and all
/// three must agree or an operator is offered a control that then refuses them,
/// which is the drift issue #444 exists to prevent.
///
/// A teammate wins when there is one. A gate has no teammate but does name the
/// workflow it belongs to, and that is the subject issue #1098 added. `None` is
/// every other native effect — something the runtime performs itself, where
/// there is no tool use to hand over and approving once is the only honest
/// answer.
pub fn subject_of(effect: &crate::ports::types::Effect) -> Option<GrantSubject> {
    if let Some(agent) = effect.agent.as_deref() {
        return Some(GrantSubject::Agent(agent.to_string()));
    }
    crate::runtime::workflow_resume::gate_workflow_id(effect)
        .map(|workflow| GrantSubject::Workflow(workflow.to_string()))
}

/// A standing permission: one tool, one subject, until a deadline (issues #374,
/// #1098).
///
/// Deliberately **not** a variant of [`GrantedCall`]. See the module docs: no
/// `args` and a non-optional expiry are the two properties that make this type
/// unable to become the thing the issue warns about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StandingGrant {
    /// This grant's id — what a revoke addresses.
    pub id: GrantId,
    /// The roster agent allowed to redeem it. Nobody else matches.
    ///
    /// Empty on a workflow permission, which names its subject in
    /// [`workflow`](Self::workflow) instead. Read
    /// [`subject`](Self::subject) rather than this field — matching on a bare
    /// agent string would let an empty one collide.
    #[serde(default)]
    pub agent: String,
    /// The authored workflow allowed to redeem it (issue #1098).
    ///
    /// `None` on every teammate permission, and on any journal line written
    /// before this field existed — both of which are agent permissions and
    /// resolve through [`agent`](Self::agent), so a replayed line reproduces
    /// today's behaviour exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// The tool it admits, with any arguments.
    pub tool: String,
    /// Whether this standing policy admits or refuses matching calls.
    #[serde(default = "default_standing_verdict")]
    pub verdict: crate::ports::types::Verdict,
    /// Who granted it. Journaled so "who opened this up" is answerable later.
    pub granted_by: Actor,
    /// The approval whose resolution minted it — the provenance the brain's
    /// re-dispatch joins on, and the audit link back to the card the operator
    /// was looking at when they decided.
    pub approval_id: ApprovalId,
    /// Epoch-millis it was minted.
    pub at_millis: u64,
    /// Absolute epoch-millis it stops admitting calls. Not an optional, and not
    /// a duration: an absolute deadline survives a restart without arithmetic,
    /// and a required field cannot be omitted into immortality.
    pub expires_at_millis: u64,
    /// The chat thread the approval was raised in (issue #379), carried for the
    /// same reason [`GrantedCall::origin_thread`] is: so the re-dispatched
    /// turn's reply is journaled back into the conversation that asked for it.
    ///
    /// **Routing only, never part of the match** — matching is `(agent, tool,
    /// unexpired)` and nothing else. A desk channel and a direct message to that
    /// channel's lead are answered by the same teammate, so without this the
    /// continuation of a channel's request would land in the lead's private
    /// line: the operator approves in one place and the work resumes in another
    /// they are not looking at.
    ///
    /// `None` for an approval with no conversation behind it, and on a grant
    /// replayed from a line written before this field existed. Both fall back to
    /// [`agent`](Self::agent), which is right for a DM.
    #[serde(default)]
    pub origin_thread: Option<String>,
    /// The thread *within* [`origin_thread`](Self::origin_thread) the approval
    /// was raised in (issue #435), carried on exactly the terms
    /// [`GrantedCall::origin_parent`] documents.
    ///
    /// **Routing only, never part of the match** — the match here is `(agent,
    /// tool, unexpired)`, and a standing grant deliberately admits any
    /// arguments; adding a location to it would make a broad permission
    /// silently narrow.
    ///
    /// `None` when the approval was raised straight in a channel, and on a
    /// grant replayed from a line written before this field existed — both
    /// answer in the channel, as before.
    #[serde(default)]
    pub origin_parent: Option<EventSeq>,
    /// The task the parked call belonged to, when raised from a task turn
    /// (issue #796), carried on exactly the terms
    /// [`GrantedCall::origin_task`] documents: routing and cleanup only, never
    /// part of the `(agent, tool, unexpired)` match. A standing grant can resume
    /// a task's checkout across repeated parks, so it carries the same link.
    ///
    /// `None` for an approval raised outside a task, and on a grant replayed
    /// from a line written before this field existed.
    #[serde(default)]
    pub origin_task: Option<String>,
    /// The slice of [`tool`](Self::tool) this grant is confined to, when the
    /// tool's name is not the whole of what it can do (issue #457).
    ///
    /// `None` for every tool whose name already describes its consequence, and
    /// for those it is not a missing value but the correct one: nothing about
    /// `file_write` needs narrowing, and matching on `(agent, tool, unexpired)`
    /// says exactly what the operator agreed to.
    ///
    /// `Some(toolkit)` for `composio_execute`, the one tool that carries every
    /// action of every connected provider under a single name. The card the
    /// operator read said "read from GitHub"; without this field the grant it
    /// minted said "make any Composio read, anywhere", because every Composio
    /// read matched the same `(agent, "composio_execute")` pair. Minted from the
    /// parked effect's own payload, so what is recorded is what was shown.
    ///
    /// Scoped by **toolkit and not by action slug**, deliberately: the operator
    /// consented to a provider, so a *different* GitHub read has to keep
    /// passing. Slug-exact would re-park every new action and make the grant
    /// worth nothing.
    ///
    /// `Some(origin)` — a `scheme://host[:port]` URL origin — for `web_fetch`
    /// since issues #673/#739, on the same terms and from the same function.
    /// **This is a second kind of value in the same string**, and a reader that
    /// assumes the toolkit kind is wrong about it: the console spelled
    /// `https://docs.rs` out with the toolkit speller and rendered
    /// `Https://docs.rs` for three releases (issue #785). Anything that
    /// *displays* a scope has to tell the two apart; anything that *matches* one
    /// must not care, because [`admits_scope`](Self::admits_scope) is exact
    /// string equality over whichever kind was minted.
    ///
    /// `None` also on a grant replayed from a journal line written before this
    /// field existed, where it means "unscoped" and reproduces the old
    /// behaviour exactly — see [`admits_scope`](Self::admits_scope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

fn default_standing_verdict() -> crate::ports::types::Verdict {
    crate::ports::types::Verdict::Approve
}

impl StandingGrant {
    /// Who may spend this permission (issue #1098).
    ///
    /// The one place the two subject fields are reconciled, so no caller has to
    /// decide what an empty [`agent`](Self::agent) means. A line carrying
    /// [`workflow`](Self::workflow) is a workflow permission; everything else —
    /// including every line written before that field existed — is an agent
    /// permission, which is what makes replay a no-op.
    pub fn subject(&self) -> GrantSubject {
        match self.workflow.as_deref() {
            Some(workflow) => GrantSubject::Workflow(workflow.to_string()),
            None => GrantSubject::Agent(self.agent.clone()),
        }
    }

    /// Whether this grant admits a call whose live scope is `scope` (issue
    /// #457).
    ///
    /// Three cases, and the two edges are the ones that matter:
    ///
    /// * **This grant has no scope** — it admits anything its `(agent, tool)`
    ///   match already admitted. That is what makes a journal line written
    ///   before the field existed replay into today's behaviour rather than
    ///   into a grant that silently stopped working.
    /// * **Scopes are equal** — the ordinary hit. A second GitHub read against a
    ///   GitHub-scoped grant.
    /// * **This grant is scoped and the live call has no scope** — refused. An
    ///   action the catalogue cannot place might belong to any provider, so
    ///   admitting it on a GitHub grant would be a guess in the permissive
    ///   direction. It falls through and parks instead, which is the same answer
    ///   this codebase gives an unrecognised action everywhere else: unknown is
    ///   a send.
    ///
    /// # Dormant, deliberately (issue #610)
    ///
    /// No current tier routes a Composio call through this: since #559 a
    /// catalogue read is allowed by the tier before the grant checks, and under
    /// `readonly` the #243 brake denies above them. The scope this spends is
    /// still minted, and the reasoning for keeping both halves is recorded once,
    /// at [`standing_scope_of`](crate::policy::consequence::standing_scope_of) —
    /// read it before concluding this is unused.
    pub fn admits_scope(&self, scope: Option<&str>) -> bool {
        match self.scope.as_deref() {
            None => true,
            Some(mine) => scope == Some(mine),
        }
    }

    /// Whether this grant still admits calls at `now_millis`.
    ///
    /// Strictly `<`, so the deadline instant itself is already past. Checked at
    /// redemption under the grant lock as well as swept periodically — the sweep
    /// is housekeeping and an operator notice, never the enforcement.
    pub fn is_live_at(&self, now_millis: u64) -> bool {
        now_millis < self.expires_at_millis
    }
}

/// The live grant set: a cheap shared handle, the same pattern as
/// [`ApprovalRequestQueue`](crate::harness::policy::ApprovalRequestQueue).
///
/// Cloning shares the state, so the policy installed on every roster agent, the
/// cycle runner that mints, and the sweep that expires all see one set.
#[derive(Clone, Default)]
pub struct GrantSet {
    inner: Arc<Mutex<GrantState>>,
    /// Serialises the mint-side opposite-polarity reconcile (issue #1458).
    ///
    /// A standing mint's `snapshot → journal revocations → insert` spans
    /// awaited journal appends, so two concurrent resolutions of the same
    /// (subject, tool, scope) with opposite verdicts — an approve and a deny
    /// landing within a few milliseconds from separate console surfaces — can
    /// both snapshot an empty opposite set and then both insert. The two
    /// opposite policies then sit live together, and because denials match
    /// first, the approve stays listed but never admits a call whatever the
    /// operator's true order. Held as a `tokio` guard across the whole sequence
    /// so the second mint sees the first's policy and supersedes it, exactly as
    /// "newest standing decision wins" promises.
    ///
    /// A `tokio::sync::Mutex` rather than the `std` one that guards the state:
    /// the guard has to survive the journal awaits inside the reconcile, which
    /// a `std::sync::MutexGuard` (not `Send`) cannot. Cloning shares the lock
    /// like it shares the state, so every holder of a cloned `GrantSet` agrees
    /// on the ordering.
    reconcile_lock: Arc<TokioMutex<()>>,
}

#[derive(Default)]
struct GrantState {
    live: HashMap<ApprovalId, GrantedCall>,
    /// Explicit request continuations, kept separate from executable grants so
    /// a denied request can never be mistaken for admitted authority.
    continuations: HashMap<ApprovalId, ApprovalContinuation>,
    /// Grants consumed since the last [`GrantSet::drain_consumed`].
    ///
    /// Consumption happens deep inside a `ToolPolicy::check`, which is sync and
    /// has no journal handle, so the record cannot be written there. The id is
    /// buffered instead and the cycle runner drains it after the cycle it
    /// belongs to. A crash between consuming and draining loses the
    /// `GrantConsumed` record, which on replay re-arms a grant whose tool
    /// already ran: the `ApprovalGranted` that minted it survives, the
    /// redemption does not, and [`GrantSet::consume`] will admit the identical
    /// call a second time with **no** new approval card — until the grant's own
    /// [`GRANT_TTL_MILLIS`] retires it.
    ///
    /// This is a duplication window, not the safe direction, and it is stated
    /// that way because it used to be recorded here as the opposite. What issue
    /// #392 could close from the journal's side it did: `GrantConsumed` is
    /// host-durable, so a record that reached the append is on stable storage
    /// before the append returns. This buffer is the half that remains — closing
    /// it means recording the redemption where it happens, which needs a journal
    /// handle at the `ToolPolicy::check` seam.
    consumed: Vec<ApprovalId>,
    /// Explicit continuations completed since the last journal drain.
    consumed_continuations: Vec<ApprovalId>,
    /// Standing grants (issue #374), keyed by their own id.
    ///
    /// A second map rather than a second variant in `live`: the two are matched
    /// on different keys (approval id vs grant id), matched by different
    /// predicates (argument-exact vs tool-and-agent), and have opposite
    /// redemption semantics (remove vs leave). Fusing them would put a branch on
    /// every operation of both.
    standing: HashMap<GrantId, StandingGrant>,
    /// Work units with an approval **parked but not yet resolved** (issue #796),
    /// keyed by the approval id so each resolution clears exactly its own entry.
    ///
    /// A parked approval mints no grant until the operator decides it, so between
    /// the park and the decision neither `live` nor `standing` names its task.
    /// Without this map [`any_for_task`](GrantSet::any_for_task) would read
    /// `false` in that window and an unrelated turn's
    /// [`sweep_orphans`](crate::harness::repo::CheckoutLedger::sweep_orphans)
    /// would delete the checkout the parked step is holding for its own resume —
    /// the very deadlock #796 exists to prevent, reopened one turn upstream.
    /// Filled when the effect parks, emptied when it resolves, is denied, or
    /// expires. In-memory only, like the checkouts it guards: a restart boot-
    /// sweeps every checkout, so there is nothing left for a rehydrated mark to
    /// protect.
    pending: HashMap<ApprovalId, String>,
}

/// Whether two recorded scopes overlap (issue #1458).
///
/// A wildcard — a scope the tool could not resolve at mint time, recorded
/// `None` — overlaps every concrete scope, because a wildcard policy matches
/// every call the tool can make. Two concrete scopes overlap only when they are
/// identical. This is deliberately not [`StandingGrant::admits_scope`], whose
/// third case refuses a scoped grant against an unresolvable live call: that is
/// the right answer for matching (unknown is a send), but the reconcile is
/// comparing two policies, not a policy and a call.
fn scopes_overlap(a: Option<&str>, b: Option<&str>) -> bool {
    a.is_none() || b.is_none() || a == b
}

impl GrantSet {
    /// Mints a grant.
    pub fn grant(&self, call: GrantedCall) {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .live
            .insert(call.approval_id.clone(), call);
    }

    /// Arms a verdict-bearing conversation continuation.
    pub fn continue_approval(&self, continuation: ApprovalContinuation) {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .continuations
            .insert(continuation.call.approval_id.clone(), continuation);
    }

    /// Redeems a grant for `(agent, tool, args)`, removing it.
    ///
    /// The match and the removal happen under one lock, so two concurrent turns
    /// racing the same grant cannot both be admitted — exactly one gets the
    /// `Some`. Returns `None` when nothing matches, which is the fall-through
    /// that re-parks.
    pub fn consume(
        &self,
        agent: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> Option<GrantedCall> {
        let mut state = self.inner.lock().expect("grant set poisoned");
        let id = state
            .live
            .iter()
            .find(|(_, g)| g.agent == agent && g.tool == tool && &g.args == args)
            .map(|(id, _)| id.clone())?;
        let call = state.live.remove(&id)?;
        state.consumed.push(id);
        Some(call)
    }

    /// Reads a live grant by approval id without redeeming it.
    ///
    /// This is how the brain recovers the arguments to tell the agent to
    /// re-issue: the grant must still be live when the instruction is written,
    /// and must still be live when the agent's tool call reaches the policy.
    pub fn peek(&self, id: &ApprovalId) -> Option<GrantedCall> {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .live
            .get(id)
            .cloned()
    }

    /// Reads an explicit approval continuation without consuming it.
    pub fn peek_continuation(&self, id: &ApprovalId) -> Option<ApprovalContinuation> {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .continuations
            .get(id)
            .cloned()
    }

    /// Consumes a completed explicit approval continuation.
    pub fn consume_continuation(&self, id: &ApprovalId) -> Option<ApprovalContinuation> {
        let mut state = self.inner.lock().expect("grant set poisoned");
        let continuation = state.continuations.remove(id)?;
        state.consumed_continuations.push(id.clone());
        Some(continuation)
    }

    /// Removes every grant minted more than `ttl_millis` before `now_millis`,
    /// returning them so the caller can journal and announce each expiry.
    pub fn sweep(&self, now_millis: u64, ttl_millis: u64) -> Vec<GrantedCall> {
        let mut state = self.inner.lock().expect("grant set poisoned");
        let expired: Vec<ApprovalId> = state
            .live
            .iter()
            .filter(|(_, g)| now_millis.saturating_sub(g.at_millis) >= ttl_millis)
            .map(|(id, _)| id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| state.live.remove(&id))
            .collect()
    }

    /// Removes explicit continuations that were never delivered before their
    /// short consent window elapsed.
    pub fn sweep_continuations(
        &self,
        now_millis: u64,
        ttl_millis: u64,
    ) -> Vec<ApprovalContinuation> {
        let mut state = self.inner.lock().expect("grant set poisoned");
        let expired: Vec<ApprovalId> = state
            .continuations
            .iter()
            .filter(|(_, c)| now_millis.saturating_sub(c.call.at_millis) >= ttl_millis)
            .map(|(id, _)| id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| state.continuations.remove(&id))
            .collect()
    }

    /// Seeds the live set from a journal replay (boot recovery).
    pub fn rehydrate(&self, calls: impl IntoIterator<Item = GrantedCall>) {
        let mut state = self.inner.lock().expect("grant set poisoned");
        for call in calls {
            state.live.insert(call.approval_id.clone(), call);
        }
    }

    /// Seeds explicit continuations from journal replay.
    pub fn rehydrate_continuations(
        &self,
        continuations: impl IntoIterator<Item = ApprovalContinuation>,
    ) {
        let mut state = self.inner.lock().expect("grant set poisoned");
        for continuation in continuations {
            state
                .continuations
                .insert(continuation.call.approval_id.clone(), continuation);
        }
    }

    /// Takes the ids consumed since the last drain, so they can be journaled.
    pub fn drain_consumed(&self) -> Vec<ApprovalId> {
        std::mem::take(&mut self.inner.lock().expect("grant set poisoned").consumed)
    }

    /// Takes explicit continuation ids completed since the last journal drain.
    pub fn drain_consumed_continuations(&self) -> Vec<ApprovalId> {
        std::mem::take(
            &mut self
                .inner
                .lock()
                .expect("grant set poisoned")
                .consumed_continuations,
        )
    }

    /// How many grants are live (tests / observability).
    pub fn live_count(&self) -> usize {
        self.inner.lock().expect("grant set poisoned").live.len()
    }

    /// Records that a work unit has an approval **parked and awaiting a
    /// decision** (issue #796), so [`any_for_task`](Self::any_for_task) treats it
    /// as live until the approval resolves.
    ///
    /// Keyed by the approval id, computed the same way the mint side derives a
    /// grant's [`GrantedCall::origin_task`], so the pending mark and the grant it
    /// eventually becomes name one unit. A task parking a second approval simply
    /// adds a second entry naming the same task; each clears independently.
    pub fn mark_pending(&self, approval_id: &ApprovalId, task: String) {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .pending
            .insert(approval_id.clone(), task);
    }

    /// Drops the pending-approval mark for `approval_id` (issue #796) — whether
    /// it was approved (a grant now names the task), denied, or expired (nothing
    /// does, so its checkout is now sweepable). A no-op for an id that never
    /// parked a task-scoped effect.
    pub fn clear_pending(&self, approval_id: &ApprovalId) {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .pending
            .remove(approval_id);
    }

    /// Whether a live grant, an **approving** standing grant, **or a
    /// still-parked approval** names `task` as its origin (issue #796).
    ///
    /// The harness asks this to decide whether a task's checkout held across an
    /// approval park is still awaiting a resume or has been orphaned by a denied
    /// or expired approval, so
    /// [`CheckoutLedger::sweep_orphans`](crate::harness::repo::CheckoutLedger::sweep_orphans)
    /// can reclaim the disk. Three states keep it live: a live grant names it (an
    /// approved step waiting to be re-issued), an **approving** standing grant
    /// names it, or an approval it parked is **still pending** — that last case
    /// mints no grant yet, so without `pending` the checkout would be swept in
    /// the window between the park and the operator's decision.
    ///
    /// A standing **denial** is deliberately not a live state (issue #1458). A
    /// denied approval is never re-dispatched to reclaim its checkout — the
    /// brain's `ApprovalResolved` arm runs only on `Approve` — so counting the
    /// deny's `origin_task` would hold the tree for the denial's full duration,
    /// up to a week, and repeated denials would accumulate disk that nothing
    /// ever resumes. A spent grant is already removed from every map, so this
    /// reads `false` the moment the resume is under way — which is safe because
    /// the resuming turn has reclaimed the tree onto its turn-scoped list by
    /// then.
    pub fn any_for_task(&self, task: &str) -> bool {
        let state = self.inner.lock().expect("grant set poisoned");
        let names_task = |t: &Option<String>| t.as_deref() == Some(task);
        state.live.values().any(|g| names_task(&g.origin_task))
            || state.standing.values().any(|g| {
                g.verdict == crate::ports::types::Verdict::Approve && names_task(&g.origin_task)
            })
            || state.pending.values().any(|t| t == task)
    }

    // -----------------------------------------------------------------------
    // Standing grants (issue #374)
    // -----------------------------------------------------------------------

    /// Holds the lock that serialises the standing-policy reconcile
    /// (issue #1458) for the caller's whole mint sequence.
    ///
    /// The standing mint takes it across `snapshot → journal → insert` — the
    /// read of [`opposite_polarity`](Self::opposite_polarity) through the write
    /// of [`grant_standing`](Self::grant_standing) — so a concurrent
    /// opposite-polarity resolution cannot interleave between them. The
    /// returned guard is `Send` (a `tokio` guard, not a `std` one), so holding
    /// it across the journal awaits inside the mint keeps the future `Send`.
    pub async fn standing_reconcile(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.reconcile_lock.lock().await
    }

    /// Arms a standing grant.
    pub fn grant_standing(&self, grant: StandingGrant) {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .standing
            .insert(grant.id.clone(), grant);
    }

    /// Matches an unexpired standing grant for `(subject, tool, scope)`
    /// **without** removing it — that is the whole difference from
    /// [`consume`](Self::consume).
    ///
    /// `scope` is the *live* call's scope, computed from its arguments by
    /// [`standing_scope_of`](crate::policy::consequence::standing_scope_of) and
    /// compared against what the grant recorded at mint time (issue #457). It is
    /// `None` for every tool whose name is the whole of what it can do, and for
    /// a Composio action the catalogue cannot place — the second of which a
    /// scoped grant refuses, per
    /// [`StandingGrant::admits_scope`](StandingGrant::admits_scope).
    ///
    /// Expiry is enforced *here*, under the same lock as the match, rather than
    /// being left to the sweep. The sweep runs on the scheduler's maintenance
    /// tick; between two ticks a lapsed grant would otherwise keep admitting
    /// calls, and "until 5pm" has to mean 5pm rather than "until the next tick
    /// after 5pm".
    ///
    /// Deterministic when several grants could match — the same agent and tool
    /// granted twice — by taking the one that expires **last**: the operator's
    /// most recent intent is the more permissive one they are living with, and
    /// picking arbitrarily out of a `HashMap` would make redemption depend on
    /// hash order.
    /// `subject` rather than a bare agent string (issue #1098): a workflow
    /// permission carries an empty [`StandingGrant::agent`], so a `&str`
    /// parameter would let one match on emptiness. The enum makes the two kinds
    /// of subject impossible to confuse at the call site.
    pub fn match_standing(
        &self,
        subject: &GrantSubject,
        tool: &str,
        scope: Option<&str>,
        now_millis: u64,
    ) -> Option<StandingGrant> {
        self.match_standing_with_verdict(
            subject,
            tool,
            scope,
            crate::ports::types::Verdict::Approve,
            now_millis,
        )
    }

    /// Matches a standing policy with the requested polarity.
    pub fn match_standing_with_verdict(
        &self,
        subject: &GrantSubject,
        tool: &str,
        scope: Option<&str>,
        verdict: crate::ports::types::Verdict,
        now_millis: u64,
    ) -> Option<StandingGrant> {
        let state = self.inner.lock().expect("grant set poisoned");
        state
            .standing
            .values()
            .filter(|g| {
                &g.subject() == subject
                    && g.tool == tool
                    && g.admits_scope(scope)
                    && g.verdict == verdict
                    && g.is_live_at(now_millis)
            })
            .max_by_key(|g| g.expires_at_millis)
            .cloned()
    }

    /// Returns every **live** standing policy of the opposite polarity whose
    /// recorded scope overlaps `grant_scope` — the same subject and tool, and a
    /// scope that either policy would shadow (issue #1458).
    ///
    /// This is the read half of the mint-side newest-decision-wins reconcile.
    /// Callers persist the returned policies as revoked before removing them
    /// with [`revoke_standing`], preserving the journal-before-live ordering
    /// used when a standing policy is minted. `ApprovalPolicy` checks a standing
    /// *denial* above a standing *grant*, so if both polarities were allowed to
    /// sit live for the same (subject, tool, scope), an approval minted after an
    /// older refusal would list as a permission and never admit a call — the
    /// operator's later decision silently inert until the refusal expired or was
    /// revoked.
    ///
    /// Scoped deliberately to policies whose scope would actually shadow the
    /// new one, so a denial of one web host does not revoke a grant for another:
    /// two policies that each govern their own slice of a tool coexist, exactly
    /// as they do when minted in isolation. Overlap is symmetric: a wildcard
    /// policy (an unresolvable scope, recorded `None`) shadows every other
    /// policy for the same tool in either direction. A wildcard old policy is
    /// superseded by any new scoped one, and a wildcard **new** policy
    /// supersedes any older scoped one — the operator's newest decision is the
    /// whole standing contract for the tool, rather than leaving the older
    /// scoped policy listed-but-inert until the wildcard expires and resurrects
    /// it.
    pub fn opposite_polarity(
        &self,
        subject: &GrantSubject,
        tool: &str,
        grant_scope: Option<&str>,
        verdict: crate::ports::types::Verdict,
        now_millis: u64,
    ) -> Vec<StandingGrant> {
        let state = self.inner.lock().expect("grant set poisoned");
        state
            .standing
            .values()
            .filter(|g| {
                g.verdict != verdict
                    && &g.subject() == subject
                    && g.tool == tool
                    && scopes_overlap(grant_scope, g.scope.as_deref())
                    && g.is_live_at(now_millis)
            })
            .cloned()
            .collect()
    }

    /// Revokes a standing grant, returning it when there was one.
    ///
    /// `None` means it was already gone — revoked by another browser tab, or
    /// swept — which the route reports as a 404 rather than pretending to have
    /// done something.
    pub fn revoke_standing(&self, id: &GrantId) -> Option<StandingGrant> {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .standing
            .remove(id)
    }

    /// Reads a standing grant by the approval that minted it.
    ///
    /// The brain's re-dispatch needs this: it is handed an approval id and has
    /// to find the permission that resolution created, whichever scope it was.
    pub fn peek_standing_by_approval(&self, approval_id: &ApprovalId) -> Option<StandingGrant> {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .standing
            .values()
            .find(|g| &g.approval_id == approval_id)
            .cloned()
    }

    /// Every live standing grant, newest first — what the console lists.
    pub fn standing(&self) -> Vec<StandingGrant> {
        let state = self.inner.lock().expect("grant set poisoned");
        let mut out: Vec<StandingGrant> = state.standing.values().cloned().collect();
        out.sort_by(|a, b| b.at_millis.cmp(&a.at_millis).then_with(|| a.id.cmp(&b.id)));
        out
    }

    /// Removes every standing grant whose deadline has passed, returning them so
    /// the caller can journal each expiry.
    pub fn sweep_standing(&self, now_millis: u64) -> Vec<StandingGrant> {
        let mut state = self.inner.lock().expect("grant set poisoned");
        let expired: Vec<GrantId> = state
            .standing
            .values()
            .filter(|g| !g.is_live_at(now_millis))
            .map(|g| g.id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| state.standing.remove(&id))
            .collect()
    }

    /// Seeds the standing map from a journal replay (boot recovery).
    pub fn rehydrate_standing(&self, grants: impl IntoIterator<Item = StandingGrant>) {
        let mut state = self.inner.lock().expect("grant set poisoned");
        for grant in grants {
            state.standing.insert(grant.id.clone(), grant);
        }
    }

    /// How many standing grants are live (tests / observability).
    pub fn standing_count(&self) -> usize {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .standing
            .len()
    }
}

impl std::fmt::Debug for GrantSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantSet")
            .field("live", &self.live_count())
            .field("standing", &self.standing_count())
            .finish_non_exhaustive()
    }
}

/// A parked "budget paused" turn (issue #1846), waiting for the operator to
/// add credits and trigger a re-issue.
///
/// Modelled on the same "mint on park, consume on redeem" shape a
/// [`GrantedCall`] uses, but simpler on two axes that follow from what a
/// budget pause actually is:
///
/// * **Matches on `(company, agent)` alone.** A grant matches a specific tool
///   call with specific arguments because approving one call must not open the
///   door to a different one; a budget pause has no call to be specific
///   about — the operator is not re-approving an action, they are re-sending
///   the message that stalled.
/// * **In-memory only, not journaled.** [`GrantSet`] is replayed on boot
///   because losing an approved-but-unredeemed grant silently drops consent
///   the operator already gave. Losing a budget-pause marker on a restart
///   costs strictly less: the operator re-sends the same message, which is
///   already the whole redemption story (issue #561: this was never going to
///   be a resume). The durability this issue asks for is "outlives the
///   request", not "survives a process restart".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetPauseMarker {
    /// Mint-order id, so a client reading the marker back can tell a fresh
    /// park from the one it already saw.
    pub id: String,
    /// The teammate whose turn paused — carried for display only; redemption
    /// re-enters the SAME cycle path an ordinary operator message takes
    /// ([`crate::ports::types::CompanyEvent::OperatorMessage`]), which routes
    /// on `chat_id` exactly as the original message did. A nested delegate's
    /// pause therefore re-issues from the top, not as a targeted re-call of
    /// that one delegate's own turn — consistent with "not true resume".
    pub agent: String,
    /// The chat/desk thread to re-dispatch on. `None` re-issues on the
    /// default (unaddressed → orchestrator) thread, matching how the original
    /// message routed.
    pub chat_id: Option<String>,
    /// The ORIGINAL message text the turn was answering — what gets re-sent,
    /// from the top, on redeem.
    pub message: String,
    /// The actionable halt copy the pause reported, carried along so a
    /// console reading the marker back — rather than the chat bubble — can
    /// still show why it exists and what it will do.
    pub summary: String,
    /// Epoch-millis the marker was parked.
    pub at_millis: u64,
    /// The ORIGINAL message's thread parent, replayed as-is on redeem (issue
    /// #1846 review, Codex #3865812423). Forcing this to `None` — the
    /// pre-fix `redeem_budget_pause` behaviour — turned a redeemed thread
    /// reply into a channel-root message: `cycle_conversation` derives both
    /// the response thread and the continuation context from this field, so
    /// the rerun and its answer landed outside the thread the pause card
    /// represented.
    pub parent: Option<EventSeq>,
    /// What the operator's composer said the ORIGINAL message was for,
    /// replayed as-is on redeem (issue #1846 review, Codex #3865812432).
    /// Forcing this to `None` — the pre-fix behaviour — changed the
    /// redeemed turn's semantics: `HarnessBrain` passes this field into
    /// `DelegationRunner::requested`, where it suppresses task creation for
    /// chat intent and enables workflow-specific behaviour, so a redeemed
    /// "Just chatting" message could unexpectedly open a card and a redeemed
    /// workflow request could be treated as an ordinary one-off.
    pub deliverable: Option<MessageIntent>,
    /// Who the ORIGINAL message named, already resolved, replayed as-is on
    /// redeem (issue #1846 review, Codex #3865812419). This event re-enters
    /// `run_cycle` directly, bypassing the REST handler's own
    /// `resolve_mentions` — forcing this to `Vec::new()` (the pre-fix
    /// behaviour) meant `HarnessBrain` read an empty vector and fell back
    /// from `mention_responder` to the desk lead/orchestrator, so adding
    /// credits could resend the task to a different agent with different
    /// tools and permissions than the operator's `@mention` had actually
    /// asked for.
    pub mentions: Vec<Mention>,
    /// The ORIGINAL message's structured attachments, replayed alongside
    /// `message` on redeem (issue #1846 review, Codex #3866418891) instead of
    /// being flattened into it a second time. Forcing this to `Vec::new()` —
    /// the pre-fix behaviour — meant the rerun journaled the
    /// `with_attachment_refs` marker text already baked into `message` as
    /// though the operator had typed it themselves: the structured attachment
    /// metadata (name/mime/size) and whatever preview the console renders
    /// from it were gone, and for a large extracted document the baked block
    /// could carry up to the wire-body limit of plain-looking transcript
    /// text. Empty for a marker with no ambient [`RedeemContext`] to draw
    /// from (a workflow node's own background turn) or whose original
    /// message carried none.
    pub attachments: Vec<Attachment>,
    /// Whether this marker's ORIGINAL turn had no chat thread an operator
    /// was addressing — a dispatched task card or a workflow agent node,
    /// rather than an interactive `/chat` message (issue #1846 review, Codex
    /// #3869193112).
    ///
    /// **Not the same question as `chat_id.is_none()`.** An ordinary
    /// interactive message sent to no specific desk ALSO parks with
    /// `chat_id: None` (see that field's doc — "unaddressed → orchestrator"
    /// is a normal, redeemable destination) and must redeem exactly as it
    /// does today. This field is the thing `chat_id` alone cannot say:
    /// whether an operator was ever in the loop for the ORIGINAL turn at
    /// all. `redeem_budget_pause` refuses when this is `true` — replaying a
    /// dispatched card's or workflow node's own turn as a generic
    /// `OperatorMessage` routes it to the orchestrator instead of the
    /// original task/node, leaving the original stuck forever while opening
    /// unrelated, possibly duplicate work.
    ///
    /// Set at the ONE call site (`run_inner` in `mod.rs`) that has the
    /// `LiveStream` value to answer this from directly; the delegation
    /// re-park call sites in `runtime/delegation.rs` do not have that
    /// context available and default to `false` — a delegated sub-turn's own
    /// background-ness is not yet distinguished by this field, which is a
    /// known gap, not a claim this covers every background origin.
    pub background: bool,
}

/// The parent / deliverable / mentions the operator's ORIGINAL message set,
/// carried ambient through a cycle so a budget-pause
/// [`park`](BudgetPauseSet::park) anywhere inside it — the top-level turn, a
/// CEO-relay call, a delegate's own turn — can stamp the marker with the
/// request the operator actually sent (issue #1846 review, Codex
/// #3865812419 / #3865812423 / #3865812432).
///
/// Set once, around the WHOLE cycle
/// ([`CycleRunner::run_bracketed`](crate::runtime::cycle::CycleRunner)),
/// from whichever event in the batch is the triggering `OperatorMessage` —
/// the same "same task, propagates through the seam" shape
/// `delegation::CHAT_ONLY_TURN` already uses for its own ambient hint, and
/// for the same reason: nothing on `run_locked`'s call chain down to a
/// `park()` site spawns onto a new task, so a `tokio::task_local!` reaches
/// every one of them without a parameter added to any function in between.
///
/// [`Default`] — no parent, no deliverable, no mentions — is the correct
/// reading for every cycle that did not start from an `OperatorMessage`: a
/// scheduler tick, a webhook, an approval follow-up. None of those has an
/// original message to replay, and a pause during one of them redeems with
/// exactly the defaults `redeem_budget_pause` used everywhere before this
/// fix.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RedeemContext {
    pub parent: Option<EventSeq>,
    pub deliverable: Option<MessageIntent>,
    pub mentions: Vec<Mention>,
    /// The ORIGINAL `OperatorMessage`'s raw text, before
    /// [`with_attachment_refs`](crate::brain::medulla::effects::with_attachment_refs)
    /// composed it with any attachment markers (issue #1846 review, Codex
    /// #3866418891) — distinct from whatever COMPOSED text a delegate's or
    /// the orchestrator's own dispatch actually ran the turn with. A
    /// `park()` call site reads this in preference to its own local message
    /// whenever it is `Some`, so a redeem re-composes fresh from raw text +
    /// [`attachments`](Self::attachments) instead of replaying a stale,
    /// already-baked block. `None` — the same "not applicable" reading
    /// [`parent`](Self::parent) already uses — for every cycle that carries
    /// no `OperatorMessage` at all (a workflow node's own background turn),
    /// where the caller's own local message is the correct thing to park.
    pub text: Option<String>,
    /// The SAME message's structured attachments, carried alongside `text`
    /// so a `park()` call site can stamp [`BudgetPauseMarker::attachments`]
    /// (issue #1846 review, Codex #3866418891). Empty whenever `text` is
    /// `None`, and also whenever the original message itself carried none.
    pub attachments: Vec<Attachment>,
}

impl RedeemContext {
    /// The context to carry for one cycle's batch — the first
    /// `OperatorMessage` in it, or [`Default`] when none is present.
    ///
    /// "First", not "every": `single_agent` already restricts an addressed
    /// batch to one agent, and every caller into `run_bracketed` (the chat
    /// route, the redeem route itself) sends exactly one `OperatorMessage`
    /// per cycle in practice. A future caller that ever batches more than
    /// one still gets a coherent, if approximate, answer rather than a
    /// panic.
    pub fn from_events(events: &[(Option<EventSeq>, CompanyEvent)]) -> Self {
        events
            .iter()
            .find_map(|(_, event)| match event {
                CompanyEvent::OperatorMessage {
                    parent,
                    deliverable,
                    mentions,
                    text,
                    attachments,
                    ..
                } => Some(Self {
                    parent: *parent,
                    deliverable: *deliverable,
                    mentions: mentions.clone(),
                    text: Some(text.clone()),
                    attachments: attachments.clone(),
                }),
                _ => None,
            })
            .unwrap_or_default()
    }
}

tokio::task_local! {
    static REDEEM_CONTEXT: RedeemContext;
}

/// Runs `fut` with [`current_redeem_context`] reading `ctx` for its
/// duration — set once around a whole cycle by
/// [`CycleRunner::run_bracketed`](crate::runtime::cycle::CycleRunner).
pub async fn with_redeem_context<F: std::future::Future>(ctx: RedeemContext, fut: F) -> F::Output {
    REDEEM_CONTEXT.scope(ctx, fut).await
}

/// The ambient [`RedeemContext`] for the cycle the caller is running inside,
/// or [`Default`] when none was set — every path that does not run through
/// [`with_redeem_context`] (every test, and any hypothetical future caller
/// of [`BudgetPauseSet::park`] outside a cycle).
pub fn current_redeem_context() -> RedeemContext {
    REDEEM_CONTEXT.try_with(Clone::clone).unwrap_or_default()
}

/// Outcome of a [`BudgetPauseSet::redeem_matching`] attempt. See that
/// method's doc comment for why an id-matched redeem exists alongside plain
/// [`BudgetPauseSet::redeem`].
#[derive(Debug, PartialEq)]
pub enum RedeemMatch {
    /// The expected marker was still parked and is now reserved.
    Reserved(BudgetPauseMarker),
    /// Nothing is parked for this agent at all.
    Absent,
    /// Something IS parked for this agent, but it is not the marker the
    /// caller expected — left untouched.
    Stale,
}

/// One company's parked budget pauses, at most one per agent (issue #1846).
///
/// Parking a new marker for an agent that already has one overwrites it: a
/// second pause on the same teammate means the first one is stale, and the
/// operator's next "add credits" should re-issue the LATEST stuck message,
/// not a queue of every message that ever stalled.
#[derive(Default)]
pub struct BudgetPauseSet {
    by_agent: Mutex<HashMap<String, BudgetPauseMarker>>,
}

impl BudgetPauseSet {
    /// Parks a fresh marker for `agent`, replacing whatever was parked
    /// before, and returns it.
    ///
    /// `redeem` is the ambient [`RedeemContext`] — pass
    /// [`current_redeem_context`] at every real call site; a bare
    /// [`RedeemContext::default`] is only correct for a test that is not
    /// itself running inside [`with_redeem_context`].
    pub fn park(
        &self,
        agent: impl Into<String>,
        chat_id: Option<String>,
        message: impl Into<String>,
        summary: impl Into<String>,
        at_millis: u64,
        redeem: RedeemContext,
    ) -> BudgetPauseMarker {
        self.park_marked(agent, chat_id, message, summary, at_millis, redeem, false)
    }

    /// [`park`](Self::park), for the ONE call site (`run_inner` in
    /// `mod.rs`) that knows — from its own `LiveStream` value — that this
    /// turn had no chat thread an operator was addressing at all: a
    /// dispatched task card or a workflow agent node (issue #1846 review,
    /// Codex #3869193112). See [`BudgetPauseMarker::background`]'s doc for
    /// why this is a different question from `chat_id.is_none()`.
    ///
    /// A SEPARATE method rather than a new parameter on [`park`](Self::park)
    /// itself: that method has ~30 call sites across this crate's own tests
    /// and the delegation re-park sites in `runtime/delegation.rs`, none of
    /// which have (or need) an opinion on this — a positional bool added
    /// there would be silent, easy-to-mis-order surface area on every one of
    /// them for a distinction only one caller actually has the information
    /// to make. Every other caller keeps calling `park`, which defaults to
    /// `background: false` — the correct answer for an interactive message
    /// (addressed or not) and the status-quo answer (no worse than before
    /// this field existed) for a delegation re-park, which does not yet
    /// have this context threaded to it either.
    pub fn park_background(
        &self,
        agent: impl Into<String>,
        chat_id: Option<String>,
        message: impl Into<String>,
        summary: impl Into<String>,
        at_millis: u64,
        redeem: RedeemContext,
    ) -> BudgetPauseMarker {
        self.park_marked(agent, chat_id, message, summary, at_millis, redeem, true)
    }

    /// Re-parks a marker for `agent` with corrected text/context — same
    /// shape as [`park`](Self::park), but carries forward whatever
    /// `background` bit is CURRENTLY set on `agent`'s marker, rather than
    /// resetting it to `false` (issue #1846 review, Codex #3870400579).
    ///
    /// For the delegation re-park sites in `runtime/delegation.rs`, which
    /// don't have their own `LiveStream` context to answer `background`
    /// from directly (see [`BudgetPauseMarker::background`]'s own doc for
    /// why plain [`park`](Self::park) there defaults to `false`): those
    /// sites run AFTER the delegate's own turn has already gone through
    /// `run_inner`, which DOES have that context and already parked (or
    /// not) a `background` marker for the SAME agent under the SAME turn,
    /// moments earlier. Reading that bit back before overwriting is what
    /// keeps a genuinely background-originated delegate turn's marker from
    /// silently losing the flag — and the redemption refusal it drives — on
    /// every delegation re-park.
    pub fn park_preserving_background(
        &self,
        agent: impl Into<String>,
        chat_id: Option<String>,
        message: impl Into<String>,
        summary: impl Into<String>,
        at_millis: u64,
        redeem: RedeemContext,
    ) -> BudgetPauseMarker {
        let agent = agent.into();
        let background = self.peek(&agent).is_some_and(|m| m.background);
        self.park_marked(
            agent, chat_id, message, summary, at_millis, redeem, background,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn park_marked(
        &self,
        agent: impl Into<String>,
        chat_id: Option<String>,
        message: impl Into<String>,
        summary: impl Into<String>,
        at_millis: u64,
        redeem: RedeemContext,
        background: bool,
    ) -> BudgetPauseMarker {
        let agent = agent.into();
        let marker = BudgetPauseMarker {
            id: generate_id(),
            agent: agent.clone(),
            chat_id,
            message: message.into(),
            summary: summary.into(),
            at_millis,
            parent: redeem.parent,
            deliverable: redeem.deliverable,
            mentions: redeem.mentions,
            attachments: redeem.attachments,
            background,
        };
        self.by_agent
            .lock()
            .expect("budget-pause set poisoned")
            .insert(agent, marker.clone());
        marker
    }

    /// Reads the parked marker for `agent` without consuming it, for a
    /// read-only console status check.
    pub fn peek(&self, agent: &str) -> Option<BudgetPauseMarker> {
        self.by_agent
            .lock()
            .expect("budget-pause set poisoned")
            .get(agent)
            .cloned()
    }

    /// Takes the parked marker for `agent`, if one exists — single-use, like
    /// a [`GrantedCall`] redemption.
    ///
    /// Also the redeem route's RESERVATION step (issue #1846 review, Codex
    /// #3865395849): taking the marker atomically, before re-dispatching,
    /// is what closes the race two concurrent redeem requests could hit —
    /// both `peek`ing the same marker before either finished, then both
    /// re-dispatching it, with only one of two later consume calls actually
    /// winning while the loser still reported success. The second caller's
    /// `redeem` here finds nothing (the first already took it) and 404s
    /// before it ever re-dispatches. [`restore_if_absent`](Self::restore_if_absent)
    /// is the undo half, for a re-dispatch that fails after reserving.
    pub fn redeem(&self, agent: &str) -> Option<BudgetPauseMarker> {
        self.by_agent
            .lock()
            .expect("budget-pause set poisoned")
            .remove(agent)
    }

    /// Reserves the parked marker for `agent` only if it is still the SAME
    /// marker as `expected_id` — otherwise leaves it untouched.
    ///
    /// Issue #1846 review (Codex #3866418876): plain [`redeem`](Self::redeem)
    /// takes WHATEVER is currently parked for the agent, which is correct
    /// for a caller with nothing to compare against, but wrong for the
    /// console's "Add credits & resend" CTA — that card is always rendered
    /// from a specific marker it already read (`GET …/budget-pause`). A
    /// background turn (a workflow node, an unstreamed task) pausing for the
    /// SAME agent re-parks with no chat destination and overwrites that
    /// marker; the console's stale-card check
    /// ([`isBudgetPauseNoticeSuperseded`] on the frontend) only watches the
    /// CHAT transcript, which a chat-less park never touches, so it cannot
    /// see this happened. A plain `redeem` would then silently reserve and
    /// re-dispatch the WRONG marker's message as though the operator had
    /// asked for it.
    ///
    /// Matching on `id` — the field [`BudgetPauseMarker::id`] documents as
    /// existing precisely "so a client reading the marker back can tell a
    /// fresh park from the one it already saw" — closes that without the
    /// console needing to know anything about chat/desk routing. Atomic
    /// under the same lock as every other operation here: a background park
    /// racing this call either lands entirely before or entirely after it,
    /// never observed half-applied.
    ///
    /// [`isBudgetPauseNoticeSuperseded`]: https://github.com/tinyhumansai/opencompany/blob/main/frontend/src/hooks/use-events.ts
    pub fn redeem_matching(&self, agent: &str, expected_id: &str) -> RedeemMatch {
        let mut by_agent = self.by_agent.lock().expect("budget-pause set poisoned");
        match by_agent.get(agent) {
            None => RedeemMatch::Absent,
            Some(marker) if marker.id != expected_id => RedeemMatch::Stale,
            Some(_) => {
                RedeemMatch::Reserved(by_agent.remove(agent).expect("just confirmed present"))
            }
        }
    }

    /// Retires the parked marker for `agent` only if its saved request
    /// context — text, chat thread, thread parent, composer intent, mentions
    /// AND attachments — still matches the candidate turn's own — otherwise
    /// leaves it untouched and returns `None`.
    ///
    /// Issue #1846 review (Codex #3869792503, tightened by #3869968949): the
    /// sibling of [`redeem_matching`](Self::redeem_matching), for the OTHER
    /// caller that retires a marker without redeeming it — `run_inner`'s
    /// "this agent's turn just succeeded without pausing" cleanup. Plain
    /// [`redeem`](Self::redeem) there retired WHATEVER was parked for the
    /// agent the moment ANY turn of theirs succeeded, which is correct for
    /// the manual-resend scenario it was written for (the very request the
    /// marker names came back and worked) but wrong when a DIFFERENT,
    /// unrelated turn for the same agent — an automatic background task, a
    /// second chat message about something else — happens to succeed first:
    /// that silently drops the marker (and its CTA) for the STILL-unretried
    /// original request, which reads to the operator as "nothing to
    /// resend" even though their original ask was never reissued.
    ///
    /// The first cut of this fix matched on [`BudgetPauseMarker::message`]
    /// alone. Text-only matching has its own gap: two DIFFERENT requests can
    /// share identical text ("review this", posted in two different threads,
    /// or with two different attachments) — the finding's own example. This
    /// widens the match to the whole saved [`RedeemContext`] a resend would
    /// have to reproduce exactly to genuinely BE the same request: `chat_id`,
    /// `parent`, `deliverable`, `mentions` and `attachments`, alongside
    /// `message`. There is still no marker id to compare against here (see
    /// `message`-only note below) — this is the closest a text/context
    /// comparison can get without one.
    ///
    /// Matching on request CONTENT rather than `id`: the caller has no
    /// marker id to compare against here (nothing was ever read back the way
    /// the console's redeem click reads one), only the shape of whatever turn
    /// just ran. A resend, by construction, runs with the SAME content the
    /// marker parked; an unrelated success does not.
    pub fn retire_if_message_matches(
        &self,
        agent: &str,
        expected_message: &str,
        expected_chat_id: Option<&str>,
        expected_redeem: &RedeemContext,
    ) -> Option<BudgetPauseMarker> {
        let mut by_agent = self.by_agent.lock().expect("budget-pause set poisoned");
        match by_agent.get(agent) {
            Some(marker)
                if marker.message == expected_message
                    && marker.chat_id.as_deref() == expected_chat_id
                    && marker.parent == expected_redeem.parent
                    && marker.deliverable == expected_redeem.deliverable
                    && marker.mentions == expected_redeem.mentions
                    && marker.attachments == expected_redeem.attachments =>
            {
                by_agent.remove(agent)
            }
            _ => None,
        }
    }

    /// Restores a marker the caller reserved via [`redeem`](Self::redeem) but
    /// whose redispatch failed to complete — guarded on absence (issue #1846
    /// review, Codex #3865395849, replacing the peek/redeem_matching shape
    /// Codex #3864988181 added).
    ///
    /// The redeem route reserves the marker with plain [`redeem`](Self::redeem)
    /// **before** re-dispatching, so a re-dispatch failure (the event store
    /// hiccups, the request is cancelled) would otherwise lose the marker for
    /// good — a retry sees nothing parked and 404s, even though no successful
    /// redispatch ever happened. This is what puts it back for the retry to
    /// find.
    ///
    /// `or_insert` rather than a plain overwrite is what makes that safe
    /// rather than merely later: the failed re-dispatch re-enters the SAME
    /// cycle path an ordinary message takes, which can itself pause again on
    /// the same agent before this call runs. Restoring unconditionally would
    /// blow away THAT fresh marker — the operator's payload for the pause
    /// that just happened, never yet shown to them — with the stale one this
    /// call is putting back. Absence-only insertion means the fresh marker,
    /// once parked, always wins.
    pub fn restore_if_absent(&self, marker: BudgetPauseMarker) {
        self.by_agent
            .lock()
            .expect("budget-pause set poisoned")
            .entry(marker.agent.clone())
            .or_insert(marker);
    }

    /// Every currently-parked marker, agent-sorted for a stable listing.
    pub fn list(&self) -> Vec<BudgetPauseMarker> {
        let mut out: Vec<_> = self
            .by_agent
            .lock()
            .expect("budget-pause set poisoned")
            .values()
            .cloned()
            .collect();
        out.sort_by(|a, b| a.agent.cmp(&b.agent));
        out
    }
}

/// Process-wide registry of [`BudgetPauseSet`]s, one per company (issue
/// #1846) — mirrors [`crate::turn_stream`]'s per-company `REGISTRY`: created
/// lazily on first use and kept for the process lifetime, since companies are
/// few and long-lived.
static BUDGET_PAUSES: LazyLock<Mutex<HashMap<CompanyId, Arc<BudgetPauseSet>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The parked-budget-pause set for `company`, creating an empty one on first
/// use.
pub fn budget_pauses_for(company: &CompanyId) -> Arc<BudgetPauseSet> {
    BUDGET_PAUSES
        .lock()
        .expect("budget-pause registry poisoned")
        .entry(company.clone())
        .or_insert_with(|| Arc::new(BudgetPauseSet::default()))
        .clone()
}

#[cfg(test)]
mod test {
    use super::*;

    fn call(id: &str, agent: &str, tool: &str, args: serde_json::Value) -> GrantedCall {
        GrantedCall {
            approval_id: ApprovalId::new(id),
            agent: agent.to_string(),
            tool: tool.to_string(),
            args,
            at_millis: 1_000,
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
        }
    }

    /// Issue #435, guarding #379's decision: a grant's origin is **routing, not
    /// identity**. Neither the channel nor the thread within it may join the
    /// redemption match.
    ///
    /// The failure this prevents is silent and expensive. If location were part
    /// of the match, a re-dispatched turn that came back anywhere other than
    /// where it started would simply fail to find its grant and re-park — the
    /// operator approves, the agent asks again, and nothing anywhere says why.
    /// `origin_parent` makes that mistake newly reachable by adding a second,
    /// finer location to get wrong, so it is pinned here rather than left to
    /// the comment on the field.
    #[test]
    fn a_grants_origin_is_routing_and_never_part_of_the_match() {
        let args = serde_json::json!({ "to": "a@b.test" });

        // Minted inside a thread; redeemed by a turn that knows only the call.
        // `consume` is not even given a location to compare against — that is
        // the shape of the guarantee.
        let set = GrantSet::default();
        set.grant(GrantedCall {
            origin_thread: Some("desk-finance".to_string()),
            origin_parent: Some(EventSeq::new(7)),
            origin_task: None,
            ..call("a1", "finance", "composio_execute", args.clone())
        });
        let redeemed = set
            .consume("finance", "composio_execute", &args)
            .expect("a thread-rooted grant is redeemed by the matching call");
        assert_eq!(
            redeemed.origin_parent,
            Some(EventSeq::new(7)),
            "and the location rides along on the consumed grant, for routing",
        );
        assert_eq!(redeemed.origin_thread, Some("desk-finance".to_string()));

        // Two grants differing *only* in origin are the same call as far as
        // matching is concerned: the first still redeems, so nothing about the
        // location narrowed it.
        for origin in [
            (None, None),
            (Some("desk-finance".to_string()), None),
            (Some("agent-cfo".to_string()), Some(EventSeq::new(9))),
        ] {
            let set = GrantSet::default();
            set.grant(GrantedCall {
                origin_thread: origin.0,
                origin_parent: origin.1,
                origin_task: None,
                ..call("a1", "finance", "composio_execute", args.clone())
            });
            assert!(
                set.consume("finance", "composio_execute", &args).is_some(),
                "the operator approved a call, not a location",
            );
        }
    }

    #[test]
    fn a_grant_is_redeemed_exactly_once() {
        let set = GrantSet::default();
        let args = serde_json::json!({ "to": "a@b.test" });
        set.grant(call("a1", "finance", "composio_execute", args.clone()));

        assert!(set.consume("finance", "composio_execute", &args).is_some());
        assert!(
            set.consume("finance", "composio_execute", &args).is_none(),
            "one approval buys one call, not an open door"
        );
        assert_eq!(set.live_count(), 0);
        assert_eq!(set.drain_consumed().len(), 1);
        assert!(set.drain_consumed().is_empty(), "the drain is a take");
    }

    #[test]
    fn a_grant_is_scoped_to_its_agent_and_its_exact_arguments() {
        let set = GrantSet::default();
        let args = serde_json::json!({ "to": "a@b.test", "body": "hi" });
        set.grant(call("a1", "finance", "composio_execute", args.clone()));

        // Another agent making the identical call is not who was approved.
        assert!(
            set.consume("marketing", "composio_execute", &args)
                .is_none()
        );
        // A different tool is not what was approved.
        assert!(set.consume("finance", "workspace_write", &args).is_none());
        // Different arguments are not what the operator saw.
        assert!(
            set.consume(
                "finance",
                "composio_execute",
                &serde_json::json!({ "to": "someone@else.test", "body": "hi" })
            )
            .is_none()
        );
        // An extra key is a different call too — matching is whole-value.
        assert!(
            set.consume(
                "finance",
                "composio_execute",
                &serde_json::json!({ "to": "a@b.test", "body": "hi", "cc": "x@y.test" })
            )
            .is_none()
        );
        // ...and none of those near-misses burned the grant.
        assert!(set.consume("finance", "composio_execute", &args).is_some());
    }

    #[test]
    fn peek_reads_without_redeeming() {
        let set = GrantSet::default();
        let args = serde_json::json!({ "q": 1 });
        set.grant(call("a1", "finance", "web_fetch", args.clone()));

        let seen = set.peek(&ApprovalId::new("a1")).expect("grant is live");
        assert_eq!(seen.tool, "web_fetch");
        assert_eq!(set.live_count(), 1, "peeking must not consume");
        assert!(set.peek(&ApprovalId::new("nope")).is_none());
    }

    /// Issue #796: the window between a park and the operator's decision. A
    /// parked approval mints no grant, so `any_for_task` would read `false`
    /// without the pending mark — and an unrelated turn's checkout sweep would
    /// then delete the parked step's tree. The mark keeps the task live until the
    /// approval settles, and two approvals on one task clear independently.
    #[test]
    fn a_still_parked_approval_keeps_its_task_alive() {
        let set = GrantSet::default();
        assert!(!set.any_for_task("t-1"), "nothing names the task yet");

        // A parked approval: no grant, but the task is marked pending.
        set.mark_pending(&ApprovalId::new("a1"), "t-1".to_string());
        assert!(
            set.any_for_task("t-1"),
            "a pending approval must keep the task live"
        );

        // A second approval parks on the same task.
        set.mark_pending(&ApprovalId::new("a2"), "t-1".to_string());
        // Settling the first still leaves the second holding the task.
        set.clear_pending(&ApprovalId::new("a1"));
        assert!(
            set.any_for_task("t-1"),
            "the second pending approval still names the task"
        );
        // Settling the last (denied or expired, so no grant follows) drops it.
        set.clear_pending(&ApprovalId::new("a2"));
        assert!(
            !set.any_for_task("t-1"),
            "no pending approval and no grant leaves nothing to keep the task alive"
        );
    }

    #[test]
    fn sweep_expires_only_grants_past_the_ttl() {
        let set = GrantSet::default();
        set.grant(call("old", "finance", "t", serde_json::json!({})));
        let mut fresh = call("new", "finance", "t2", serde_json::json!({}));
        fresh.at_millis = 900_000;
        set.grant(fresh);

        let expired = set.sweep(1_000 + GRANT_TTL_MILLIS, GRANT_TTL_MILLIS);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].approval_id, ApprovalId::new("old"));
        assert_eq!(set.live_count(), 1, "the fresh grant survives");
    }

    #[test]
    fn rehydrate_seeds_the_live_set() {
        let set = GrantSet::default();
        set.rehydrate([
            call("a1", "finance", "t", serde_json::json!({})),
            call("a2", "legal", "t", serde_json::json!({})),
        ]);
        assert_eq!(set.live_count(), 2);
        assert!(set.peek(&ApprovalId::new("a2")).is_some());
    }

    /// Concurrent redemption of one grant: exactly one caller wins.
    ///
    /// The match and the removal are one critical section precisely so this
    /// cannot double-fire. A read-then-remove would let both threads see the
    /// grant and both proceed, turning one approval into two executions of a
    /// tool the operator approved once.
    #[test]
    fn two_threads_racing_one_grant_yield_exactly_one_winner() {
        let set = GrantSet::default();
        let args = serde_json::json!({ "amount_usd": 40.0 });
        set.grant(call("a1", "finance", "pay_invoice", args.clone()));

        let barrier = Arc::new(std::sync::Barrier::new(8));
        let winners = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let set = set.clone();
                let args = args.clone();
                let barrier = Arc::clone(&barrier);
                let winners = Arc::clone(&winners);
                std::thread::spawn(move || {
                    barrier.wait();
                    if set.consume("finance", "pay_invoice", &args).is_some() {
                        winners.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread");
        }

        assert_eq!(winners.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(set.live_count(), 0);
        assert_eq!(set.drain_consumed().len(), 1, "one consumption journaled");
    }

    // -----------------------------------------------------------------------
    // Standing grants (issue #374)
    // -----------------------------------------------------------------------

    fn operator() -> Actor {
        Actor {
            kind: crate::ports::types::ActorKind::User,
            id: "user-1".to_string(),
        }
    }

    fn standing(id: &str, agent: &str, tool: &str, expires_at_millis: u64) -> StandingGrant {
        StandingGrant {
            id: GrantId::new(id),
            agent: agent.to_string(),
            workflow: None,
            tool: tool.to_string(),
            verdict: crate::ports::types::Verdict::Approve,
            granted_by: operator(),
            approval_id: ApprovalId::new(format!("approval-{id}")),
            at_millis: 1_000,
            expires_at_millis,
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
            scope: None,
        }
    }

    /// A permission held by a workflow rather than a teammate (issue #1098).
    fn standing_workflow(
        id: &str,
        workflow: &str,
        tool: &str,
        scope: Option<&str>,
        expires_at_millis: u64,
    ) -> StandingGrant {
        StandingGrant {
            agent: String::new(),
            workflow: Some(workflow.to_string()),
            scope: scope.map(str::to_string),
            ..standing(id, "", tool, expires_at_millis)
        }
    }

    /// The two subject fields reconcile in exactly one place, and a line with no
    /// `workflow` is an agent permission — which is what makes every journal line
    /// written before issue #1098 replay unchanged.
    #[test]
    fn subject_reads_a_workflow_line_as_a_workflow_and_everything_else_as_an_agent() {
        assert_eq!(
            standing("g1", "maya", "web_fetch", 9_999).subject(),
            GrantSubject::agent("maya")
        );
        assert_eq!(
            standing_workflow("g2", "sports_blog", "web_fetch", None, 9_999).subject(),
            GrantSubject::workflow("sports_blog")
        );
    }

    /// A journal line written before the field existed carries no `workflow` key
    /// at all. It must deserialize, and it must replay as the agent permission it
    /// was — not as a workflow one keyed on an empty string.
    #[test]
    fn a_pre_1098_journal_line_replays_as_an_agent_permission() {
        let line = r#"{
            "id": "g-old",
            "agent": "maya",
            "tool": "web_fetch",
            "granted_by": { "kind": "user", "id": "user-1" },
            "approval_id": "approval-old",
            "at_millis": 1000,
            "expires_at_millis": 9999
        }"#;
        let replayed: StandingGrant =
            serde_json::from_str(line).expect("a pre-#1098 line still deserializes");
        assert_eq!(replayed.workflow, None);
        assert_eq!(replayed.subject(), GrantSubject::agent("maya"));

        let set = GrantSet::default();
        set.grant_standing(replayed);
        assert!(
            set.match_standing(&GrantSubject::agent("maya"), "web_fetch", None, 2_000)
                .is_some(),
            "a replayed line must still admit the calls it always admitted"
        );
    }

    /// The two subjects are separate namespaces. A workflow named like a teammate
    /// must not spend that teammate's permission, in either direction.
    #[test]
    fn an_agent_and_a_workflow_of_the_same_name_do_not_share_a_permission() {
        let set = GrantSet::default();
        set.grant_standing(standing("g1", "digest", "web_fetch", 9_999));

        assert!(
            set.match_standing(&GrantSubject::workflow("digest"), "web_fetch", None, 2_000)
                .is_none(),
            "a workflow must not spend a teammate's permission"
        );

        let set = GrantSet::default();
        set.grant_standing(standing_workflow("g2", "digest", "web_fetch", None, 9_999));
        assert!(
            set.match_standing(&GrantSubject::agent("digest"), "web_fetch", None, 2_000)
                .is_none(),
            "a teammate must not spend a workflow's permission"
        );
    }

    /// The scope machinery is subject-agnostic: a workflow permission narrows by
    /// host on exactly the terms a teammate's does.
    #[test]
    fn a_workflow_permission_is_narrowed_by_its_host_like_any_other() {
        let set = GrantSet::default();
        set.grant_standing(standing_workflow(
            "g1",
            "sports_blog",
            "web_fetch",
            Some("https://www.bbc.co.uk"),
            9_999,
        ));
        let subject = GrantSubject::workflow("sports_blog");

        assert!(
            set.match_standing(&subject, "web_fetch", Some("https://www.bbc.co.uk"), 2_000)
                .is_some(),
            "the host it was granted for keeps passing"
        );
        assert!(
            set.match_standing(&subject, "web_fetch", Some("https://www.espn.com"), 2_000)
                .is_none(),
            "a repointed host re-parks — scope equality is the invalidation"
        );
        assert!(
            set.match_standing(&subject, "web_fetch", None, 2_000)
                .is_none(),
            "a call whose host cannot be read is refused by a scoped permission"
        );
    }

    /// A grant confined to one Composio toolkit (issue #457).
    fn scoped(
        id: &str,
        agent: &str,
        tool: &str,
        scope: &str,
        expires_at_millis: u64,
    ) -> StandingGrant {
        StandingGrant {
            scope: Some(scope.to_string()),
            ..standing(id, agent, tool, expires_at_millis)
        }
    }

    /// The whole point of the scope: the tool stops asking, whatever the
    /// arguments, until the deadline.
    #[test]
    fn a_standing_grant_admits_varying_arguments_until_it_expires() {
        let set = GrantSet::default();
        set.grant_standing(standing("g1", "ops", "shell", 10_000));

        assert!(
            set.match_standing(&GrantSubject::agent("ops"), "shell", None, 2_000)
                .is_some()
        );
        // Matching does not depend on arguments at all — there are none to
        // depend on. Two different calls, same admission.
        assert!(
            set.match_standing(&GrantSubject::agent("ops"), "shell", None, 9_999)
                .is_some()
        );
        assert_eq!(
            set.standing_count(),
            1,
            "redeeming a standing grant must not remove it — that is single-use's job"
        );

        // The deadline instant itself is already past.
        assert!(
            set.match_standing(&GrantSubject::agent("ops"), "shell", None, 10_000)
                .is_none()
        );
        assert!(
            set.match_standing(&GrantSubject::agent("ops"), "shell", None, 10_001)
                .is_none()
        );
    }

    #[test]
    fn a_standing_deny_matches_only_its_subject_tool_and_deadline() {
        let set = GrantSet::default();
        let mut deny = standing("deny-1", "ops", "shell", 10_000);
        deny.verdict = crate::ports::types::Verdict::Deny;
        set.grant_standing(deny);

        assert!(
            set.match_standing_with_verdict(
                &GrantSubject::agent("ops"),
                "shell",
                None,
                crate::ports::types::Verdict::Deny,
                2_000,
            )
            .is_some()
        );
        assert!(
            set.match_standing_with_verdict(
                &GrantSubject::agent("ops"),
                "shell",
                None,
                crate::ports::types::Verdict::Deny,
                10_000,
            )
            .is_none()
        );
        assert!(
            set.match_standing_with_verdict(
                &GrantSubject::agent("ops"),
                "shell",
                None,
                crate::ports::types::Verdict::Approve,
                2_000,
            )
            .is_none()
        );
    }

    /// Issue #1458: a standing **denial** must not keep a task's checkout alive.
    ///
    /// A denied approval is never re-dispatched — the brain's `ApprovalResolved`
    /// arm runs only on `Approve` — so nothing will ever reclaim the held tree,
    /// and counting the deny's `origin_task` would retain it for the denial's
    /// full duration. Only an approving grant names a task that may still resume.
    #[test]
    fn a_standing_deny_does_not_keep_a_task_checkout_alive() {
        let set = GrantSet::default();
        let mut deny = standing("deny-1", "ops", "shell", 10_000);
        deny.verdict = crate::ports::types::Verdict::Deny;
        deny.origin_task = Some("t-1".to_string());
        set.grant_standing(deny);
        assert!(
            !set.any_for_task("t-1"),
            "a deny is never resumed, so it must not hold the task's checkout"
        );

        // An approving standing grant for the same task is still a live reason.
        let mut grant = standing("grant-1", "ops", "shell", 10_000);
        grant.origin_task = Some("t-1".to_string());
        set.grant_standing(grant);
        assert!(
            set.any_for_task("t-1"),
            "an approving standing grant still keeps the checkout live"
        );
    }

    /// Issue #1458, the reconcile: newest standing decision wins. An approval
    /// minted after a live denial of the same scope takes the denial back, or
    /// `ApprovalPolicy`'s deny-above-grant ordering would leave the operator's
    /// later "yes" listed but never admitting a call.
    #[test]
    fn an_approval_mint_supersedes_a_live_denial_of_the_same_scope() {
        let set = GrantSet::default();
        let mut deny = scoped("deny-1", "ops", "web_fetch", "https://docs.rs", 10_000);
        deny.verdict = crate::ports::types::Verdict::Deny;
        set.grant_standing(deny);

        let drained = set.opposite_polarity(
            &GrantSubject::agent("ops"),
            "web_fetch",
            Some("https://docs.rs"),
            crate::ports::types::Verdict::Approve,
            2_000,
        );
        assert_eq!(drained.len(), 1, "the shadowing denial is taken back");
        assert_eq!(drained[0].id, GrantId::new("deny-1"));
        assert!(
            set.match_standing(
                &GrantSubject::agent("ops"),
                "web_fetch",
                Some("https://docs.rs"),
                2_000
            )
            .is_none(),
            "only the new approval is left for this scope"
        );
    }

    /// The mirror direction: a denial minted after a live approval of the same
    /// scope takes the grant back, so the operator's newer refusal is the whole
    /// of the standing contract.
    #[test]
    fn a_denial_mint_supersedes_a_live_approval_of_the_same_scope() {
        let set = GrantSet::default();
        set.grant_standing(scoped(
            "grant-1",
            "ops",
            "web_fetch",
            "https://docs.rs",
            10_000,
        ));

        let drained = set.opposite_polarity(
            &GrantSubject::agent("ops"),
            "web_fetch",
            Some("https://docs.rs"),
            crate::ports::types::Verdict::Deny,
            2_000,
        );
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, GrantId::new("grant-1"));
        set.revoke_standing(&drained[0].id);
        assert_eq!(set.standing().len(), 0);
    }

    /// Opposite polarities for *different* scopes coexist: a denial of one host
    /// neither shadows nor takes back a grant for another, exactly as they would
    /// if minted in isolation.
    #[test]
    fn a_denial_for_one_host_does_not_take_back_a_grant_for_another() {
        let set = GrantSet::default();
        set.grant_standing(scoped(
            "grant-1",
            "ops",
            "web_fetch",
            "https://docs.rs",
            10_000,
        ));
        let mut deny = scoped(
            "deny-1",
            "ops",
            "web_fetch",
            "https://other.example",
            10_000,
        );
        deny.verdict = crate::ports::types::Verdict::Deny;
        set.grant_standing(deny);

        let drained = set.opposite_polarity(
            &GrantSubject::agent("ops"),
            "web_fetch",
            Some("https://docs.rs"),
            crate::ports::types::Verdict::Approve,
            2_000,
        );
        assert!(
            drained.is_empty(),
            "a denial for other.example does not shadow a docs.rs approval"
        );
        assert_eq!(set.standing().len(), 2);
    }

    /// A wildcard old policy (a scope the tool could not resolve, recorded
    /// `None`) shadows *every* new policy for the same tool, so it is
    /// superseded too — the newer decision is the whole of the contract.
    #[test]
    fn a_wildcard_denial_is_superseded_by_any_new_policy_for_the_same_tool() {
        let set = GrantSet::default();
        let mut deny = standing("deny-1", "ops", "web_fetch", 10_000); // scope None
        deny.verdict = crate::ports::types::Verdict::Deny;
        set.grant_standing(deny);

        let drained = set.opposite_polarity(
            &GrantSubject::agent("ops"),
            "web_fetch",
            Some("https://docs.rs"),
            crate::ports::types::Verdict::Approve,
            2_000,
        );
        assert_eq!(drained.len(), 1);
    }

    /// The mirror of the wildcard-old case: a **new** wildcard policy (an
    /// unresolvable scope, recorded `None`) shadows every scoped opposite policy
    /// for the same tool, so the reconcile takes the older scoped one too.
    /// Otherwise it would sit listed-but-inert while the wildcard refused every
    /// call, and silently resurrect when the wildcard expired — the newest
    /// standing decision should be the whole of the contract.
    #[test]
    fn a_new_wildcard_policy_supersedes_an_older_scoped_opposite() {
        let set = GrantSet::default();
        set.grant_standing(scoped(
            "approve-1",
            "ops",
            "web_fetch",
            "https://docs.rs",
            10_000,
        ));

        let drained = set.opposite_polarity(
            &GrantSubject::agent("ops"),
            "web_fetch",
            None, // a scope the tool could not resolve
            crate::ports::types::Verdict::Deny,
            2_000,
        );
        assert_eq!(
            drained.len(),
            1,
            "the new wildcard refusal supersedes the older scoped approval"
        );
        assert_eq!(drained[0].id, GrantId::new("approve-1"));
        assert_eq!(
            set.standing().len(),
            1,
            "the reconcile is read-only; the caller persists the revocation before revoking"
        );
    }

    #[test]
    fn same_polarity_policies_are_never_reconciled() {
        let set = GrantSet::default();
        set.grant_standing(scoped("g1", "ops", "web_fetch", "https://docs.rs", 10_000));

        let drained = set.opposite_polarity(
            &GrantSubject::agent("ops"),
            "web_fetch",
            Some("https://docs.rs"),
            crate::ports::types::Verdict::Approve,
            2_000,
        );
        assert!(drained.is_empty());
        assert_eq!(set.standing().len(), 1);
    }

    /// An expired opposite-polarity policy shadows nothing — the matcher
    /// refuses it — so the reconcile leaves it for the sweep.
    #[test]
    fn an_expired_opposite_polarity_policy_is_left_for_the_sweep() {
        let set = GrantSet::default();
        let mut deny = scoped("deny-1", "ops", "web_fetch", "https://docs.rs", 5_000);
        deny.verdict = crate::ports::types::Verdict::Deny;
        set.grant_standing(deny);

        let drained = set.opposite_polarity(
            &GrantSubject::agent("ops"),
            "web_fetch",
            Some("https://docs.rs"),
            crate::ports::types::Verdict::Approve,
            6_000,
        );
        assert!(drained.is_empty());
        assert_eq!(set.standing().len(), 1);
    }

    #[test]
    fn a_standing_grant_is_scoped_to_its_agent_and_its_tool() {
        let set = GrantSet::default();
        set.grant_standing(standing("g1", "ops", "shell", 10_000));

        assert!(
            set.match_standing(&GrantSubject::agent("marketing"), "shell", None, 2_000)
                .is_none(),
            "another teammate is not who the operator granted"
        );
        assert!(
            set.match_standing(&GrantSubject::agent("ops"), "workspace_write", None, 2_000)
                .is_none(),
            "another tool is not what the operator granted"
        );
    }

    /// Issue #457, the discrimination that matters. A grant minted from a
    /// GitHub read admits another GitHub read — the operator consented to a
    /// provider, not to one action — and refuses a Gmail read. Both are
    /// catalogue reads, so nothing upstream of this predicate tells them apart:
    /// same agent, same tool, same grantable verdict.
    #[test]
    fn a_scoped_grant_admits_its_own_provider_and_refuses_another() {
        let set = GrantSet::default();
        set.grant_standing(scoped("g1", "ops", "composio_execute", "github", 10_000));

        assert!(
            set.match_standing(
                &GrantSubject::agent("ops"),
                "composio_execute",
                Some("github"),
                2_000
            )
            .is_some(),
            "a second GitHub read is the sentence the operator agreed to"
        );
        assert!(
            set.match_standing(
                &GrantSubject::agent("ops"),
                "composio_execute",
                Some("gmail"),
                2_000
            )
            .is_none(),
            "'read from GitHub' is not consent to read a mailbox"
        );
    }

    /// A call the catalogue cannot place has no scope, and a scoped grant must
    /// not admit it: it could belong to any provider, and guessing would guess
    /// permissively. It falls through and parks, which is what this codebase
    /// does with every unrecognised action.
    #[test]
    fn a_scoped_grant_refuses_a_call_with_no_scope_at_all() {
        let set = GrantSet::default();
        set.grant_standing(scoped("g1", "ops", "composio_execute", "github", 10_000));

        assert!(
            set.match_standing(&GrantSubject::agent("ops"), "composio_execute", None, 2_000)
                .is_none()
        );
    }

    /// **Replay compatibility (issue #457).** A `StandingGrantMinted` line
    /// written before the scope field existed deserializes with `scope: None`,
    /// and an unscoped grant behaves exactly as it did before this change —
    /// admitting the tool whatever the live scope is. Without this the upgrade
    /// would silently void every permission an operator granted before it.
    #[test]
    fn a_grant_journaled_before_scopes_existed_replays_and_behaves_as_before() {
        // The old wire shape, verbatim: no `scope` key anywhere.
        let line = serde_json::json!({
            "id": "g-old",
            "agent": "ops",
            "tool": "composio_execute",
            "granted_by": { "kind": "user", "id": "user-1" },
            "approval_id": "approval-g-old",
            "at_millis": 1_000,
            "expires_at_millis": 10_000,
        });
        let replayed: StandingGrant =
            serde_json::from_value(line).expect("an old line still deserializes");
        assert_eq!(replayed.scope, None, "absent means unscoped, not broken");

        let set = GrantSet::default();
        set.rehydrate_standing([replayed]);

        // Unscoped: it admits the tool exactly as it did before scopes existed,
        // whatever the live call resolves to.
        for live in [Some("github"), Some("gmail"), None] {
            assert!(
                set.match_standing(&GrantSubject::agent("ops"), "composio_execute", live, 2_000)
                    .is_some(),
                "an unscoped grant must keep admitting: {live:?}"
            );
        }
        // …and the boundaries it always had still hold.
        assert!(
            set.match_standing(
                &GrantSubject::agent("marketing"),
                "composio_execute",
                Some("github"),
                2_000
            )
            .is_none()
        );
        assert!(
            set.match_standing(
                &GrantSubject::agent("ops"),
                "composio_execute",
                Some("github"),
                10_000
            )
            .is_none()
        );
    }

    /// A scoped grant round-trips, and an unscoped one still writes the old
    /// shape — so a journal read by an older build is unchanged.
    #[test]
    fn the_scope_round_trips_and_is_omitted_when_absent() {
        let unscoped = serde_json::to_value(standing("g1", "ops", "shell", 10_000)).expect("json");
        assert!(
            unscoped.get("scope").is_none(),
            "an unscoped grant writes the pre-#457 line: {unscoped}"
        );

        let grant = scoped("g2", "ops", "composio_execute", "github", 10_000);
        let round: StandingGrant =
            serde_json::from_value(serde_json::to_value(&grant).expect("json"))
                .expect("round trip");
        assert_eq!(round, grant);
    }

    #[test]
    fn revoking_a_standing_grant_stops_it_matching() {
        let set = GrantSet::default();
        set.grant_standing(standing("g1", "ops", "shell", 10_000));
        assert!(
            set.match_standing(&GrantSubject::agent("ops"), "shell", None, 2_000)
                .is_some()
        );

        let revoked = set.revoke_standing(&GrantId::new("g1")).expect("was live");
        assert_eq!(revoked.tool, "shell");
        assert!(
            set.match_standing(&GrantSubject::agent("ops"), "shell", None, 2_000)
                .is_none()
        );
        assert_eq!(set.standing_count(), 0);
        assert!(
            set.revoke_standing(&GrantId::new("g1")).is_none(),
            "revoking twice reports nothing to revoke rather than pretending"
        );
    }

    /// A single-use grant must burn even when a standing grant would also have
    /// admitted the call.
    ///
    /// The ordering is enforced by the policy arm, but the primitives have to
    /// make it expressible: `consume` is what removes, and a standing match
    /// never touches the single-use set. If a standing match ran first, the
    /// operator's one-off approval would sit live until its TTL and then be
    /// announced as "the agent never acted" — a lie about work that ran.
    #[test]
    fn a_single_use_grant_still_burns_while_a_standing_grant_is_live() {
        let set = GrantSet::default();
        let args = serde_json::json!({ "cmd": "ls" });
        set.grant(call("a1", "ops", "shell", args.clone()));
        set.grant_standing(standing("g1", "ops", "shell", 10_000));

        assert!(set.consume("ops", "shell", &args).is_some());
        assert_eq!(set.live_count(), 0, "the single-use grant burned");
        assert_eq!(set.standing_count(), 1, "the standing grant is untouched");
        assert_eq!(set.drain_consumed().len(), 1);
    }

    #[test]
    fn sweep_standing_removes_only_lapsed_grants() {
        let set = GrantSet::default();
        set.grant_standing(standing("old", "ops", "shell", 5_000));
        set.grant_standing(standing("new", "ops", "workspace_write", 50_000));

        let expired = set.sweep_standing(10_000);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, GrantId::new("old"));
        assert_eq!(set.standing_count(), 1);
        assert!(
            set.match_standing(&GrantSubject::agent("ops"), "workspace_write", None, 10_000)
                .is_some()
        );
    }

    #[test]
    fn the_longest_lived_match_wins_so_redemption_is_not_hash_order() {
        let set = GrantSet::default();
        set.grant_standing(standing("short", "ops", "shell", 5_000));
        set.grant_standing(standing("long", "ops", "shell", 50_000));

        let matched = set
            .match_standing(&GrantSubject::agent("ops"), "shell", None, 1_000)
            .expect("matches");
        assert_eq!(matched.id, GrantId::new("long"));
    }

    #[test]
    fn standing_grants_rehydrate_and_are_findable_by_their_approval() {
        let set = GrantSet::default();
        set.rehydrate_standing([
            standing("g1", "ops", "shell", 10_000),
            standing("g2", "legal", "workspace_write", 10_000),
        ]);
        assert_eq!(set.standing_count(), 2);

        let found = set
            .peek_standing_by_approval(&ApprovalId::new("approval-g2"))
            .expect("provenance is queryable");
        assert_eq!(found.agent, "legal");
        assert!(
            set.peek_standing_by_approval(&ApprovalId::new("nope"))
                .is_none()
        );

        // Newest first, and `at_millis` ties break on id so the list is stable.
        let listed = set.standing();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, GrantId::new("g1"));
    }

    // --- budget-pause markers (issue #1846) ----------------------------------

    #[test]
    fn parking_then_peeking_a_budget_pause_does_not_consume_it() {
        let set = BudgetPauseSet::default();
        set.park(
            "ceo",
            Some("desk-1".to_string()),
            "hi",
            "paused",
            1_000,
            RedeemContext::default(),
        );

        let first = set.peek("ceo").expect("parked");
        let second = set.peek("ceo").expect("peek does not consume");
        assert_eq!(first.id, second.id, "the same marker both times");
        assert_eq!(first.message, "hi");
        assert_eq!(first.chat_id.as_deref(), Some("desk-1"));
    }

    /// Issue #1846 review (Codex #3870400579) — **the regression.** A
    /// delegation re-park (`runtime/delegation.rs`) has no `LiveStream`
    /// context of its own to answer `background` from, so it must carry
    /// forward whatever the delegate's own `run_inner` call already set,
    /// rather than resetting it to `false` via plain `park`.
    #[test]
    fn park_preserving_background_carries_the_flag_forward() {
        let set = BudgetPauseSet::default();
        set.park_background(
            "ceo",
            None,
            "the hand-off instruction",
            "paused",
            1_000,
            RedeemContext::default(),
        );
        assert!(
            set.peek("ceo").expect("parked").background,
            "fixture sanity: the marker `run_inner` would have parked is background"
        );

        let reparked = set.park_preserving_background(
            "ceo",
            None,
            "the corrected original request",
            "paused",
            2_000,
            RedeemContext::default(),
        );
        assert!(
            reparked.background,
            "re-parking with corrected text must not silently reset background to false"
        );
        assert_eq!(reparked.message, "the corrected original request");
    }

    /// The other half: an ordinary (non-background) marker's re-park must
    /// stay non-background — this method is not a way to ACCIDENTALLY turn
    /// an interactive marker into a background one either.
    #[test]
    fn park_preserving_background_leaves_a_non_background_marker_alone() {
        let set = BudgetPauseSet::default();
        set.park(
            "ceo",
            Some("general".to_string()),
            "hi",
            "paused",
            1_000,
            RedeemContext::default(),
        );
        assert!(!set.peek("ceo").expect("parked").background);

        let reparked = set.park_preserving_background(
            "ceo",
            Some("general".to_string()),
            "hi, corrected",
            "paused",
            2_000,
            RedeemContext::default(),
        );
        assert!(!reparked.background);
    }

    /// No prior marker to read a flag off of — defaults to `false`, the
    /// same as plain `park`, rather than panicking or guessing `true`.
    #[test]
    fn park_preserving_background_defaults_to_false_with_nothing_parked_yet() {
        let set = BudgetPauseSet::default();
        let marker = set.park_preserving_background(
            "ceo",
            None,
            "hi",
            "paused",
            1_000,
            RedeemContext::default(),
        );
        assert!(!marker.background);
    }

    #[test]
    fn redeeming_a_budget_pause_consumes_it_exactly_once() {
        let set = BudgetPauseSet::default();
        set.park("ceo", None, "hi", "paused", 1_000, RedeemContext::default());

        let redeemed = set.redeem("ceo").expect("a marker was parked");
        assert_eq!(redeemed.agent, "ceo");
        assert!(
            set.redeem("ceo").is_none(),
            "single-use: a second redeem finds nothing"
        );
        assert!(set.peek("ceo").is_none());
    }

    #[test]
    fn redeem_reserves_atomically_so_a_second_concurrent_redeem_finds_nothing() {
        // Issue #1846 review (Codex #3865395849): two redeem requests that
        // both read the marker before either re-dispatches would both
        // re-dispatch the same non-idempotent message. Reserving with plain
        // `redeem` up front — rather than `peek`, then consume after — means
        // the SECOND caller's own `redeem` finds nothing, closing the race
        // before it ever gets to re-dispatch.
        let set = BudgetPauseSet::default();
        let marker = set.park("ceo", None, "hi", "paused", 1_000, RedeemContext::default());

        let first = set.redeem("ceo").expect("the first reservation wins");
        assert_eq!(first.id, marker.id);
        assert!(
            set.redeem("ceo").is_none(),
            "a second, concurrent reservation finds nothing parked"
        );
    }

    #[test]
    fn restore_if_absent_puts_a_failed_redispatchs_marker_back() {
        let set = BudgetPauseSet::default();
        let marker = set.park("ceo", None, "hi", "paused", 1_000, RedeemContext::default());

        let reserved = set.redeem("ceo").expect("reserved for redispatch");
        assert!(set.peek("ceo").is_none(), "reserved out of the set");

        // The redispatch failed — restore it for a retry to find.
        set.restore_if_absent(reserved);
        let restored = set.peek("ceo").expect("restored after the failure");
        assert_eq!(restored.id, marker.id);
        assert_eq!(restored.message, "hi");
    }

    #[test]
    fn restore_if_absent_leaves_a_fresher_marker_untouched() {
        // The re-dispatch a redeem triggers can itself re-pause the same
        // agent before the failed-redispatch's restore call runs. The stale
        // marker being restored must not delete the operator's not-yet-seen
        // fresh one out from under them.
        let set = BudgetPauseSet::default();
        let stale = set.park(
            "ceo",
            None,
            "first stuck message",
            "paused once",
            1_000,
            RedeemContext::default(),
        );
        let reserved = set.redeem("ceo").expect("reserved for redispatch");
        assert_eq!(reserved.id, stale.id);

        // The (failed) redispatch itself re-entered the cycle and paused
        // again before the restore call below runs.
        set.park(
            "ceo",
            None,
            "second stuck message",
            "paused again",
            2_000,
            RedeemContext::default(),
        );

        set.restore_if_absent(reserved);
        let still_parked = set.peek("ceo").expect("the fresher marker survives");
        assert_eq!(still_parked.message, "second stuck message");
    }

    #[test]
    fn a_second_pause_on_the_same_agent_overwrites_the_first() {
        let set = BudgetPauseSet::default();
        set.park(
            "ceo",
            None,
            "first stuck message",
            "paused once",
            1_000,
            RedeemContext::default(),
        );
        set.park(
            "ceo",
            None,
            "second stuck message",
            "paused again",
            2_000,
            RedeemContext::default(),
        );

        let marker = set.redeem("ceo").expect("the latest marker");
        assert_eq!(
            marker.message, "second stuck message",
            "the operator's next redeem re-issues the LATEST stalled message, not a queue"
        );
    }

    #[test]
    fn redeem_matching_reserves_when_the_id_still_matches() {
        let set = BudgetPauseSet::default();
        let marker = set.park("ceo", None, "hi", "paused", 1_000, RedeemContext::default());

        let outcome = set.redeem_matching("ceo", &marker.id);
        assert_eq!(outcome, RedeemMatch::Reserved(marker));
        assert!(set.peek("ceo").is_none(), "reserved out of the set");
    }

    #[test]
    fn redeem_matching_reports_absent_when_nothing_is_parked() {
        let set = BudgetPauseSet::default();
        assert_eq!(set.redeem_matching("ceo", "some-id"), RedeemMatch::Absent);
    }

    #[test]
    fn redeem_matching_leaves_a_background_overwrite_untouched_on_a_stale_id() {
        // Issue #1846 review (Codex #3866418876): a chat pause parks a
        // marker with a chat destination; a background turn (workflow node,
        // unstreamed task) for the SAME agent then pauses too and overwrites
        // it with a marker that has NONE. The console still shows the OLD
        // chat card because nothing about a chat-less park touches the
        // transcript-based staleness check. Redeeming by the OLD id must
        // not silently take the NEW (unrelated) marker.
        let set = BudgetPauseSet::default();
        let chat_marker = set.park(
            "ceo",
            Some("general".to_string()),
            "ship the API",
            "paused for the chat turn",
            1_000,
            RedeemContext::default(),
        );
        let background_marker = set.park(
            "ceo",
            None,
            "run the nightly workflow node",
            "paused for the background turn",
            2_000,
            RedeemContext::default(),
        );

        // The stale chat id must not reserve the background marker.
        assert_eq!(
            set.redeem_matching("ceo", &chat_marker.id),
            RedeemMatch::Stale
        );
        // Left completely untouched — still there, still the background one.
        let still_parked = set.peek("ceo").expect("the background marker survives");
        assert_eq!(still_parked.id, background_marker.id);
        assert_eq!(still_parked.message, "run the nightly workflow node");

        // The fresh id reserves correctly.
        let outcome = set.redeem_matching("ceo", &background_marker.id);
        assert_eq!(outcome, RedeemMatch::Reserved(background_marker));
        assert!(set.peek("ceo").is_none());
    }

    #[test]
    fn redeem_matching_reserves_atomically_so_a_concurrent_stale_attempt_finds_nothing() {
        let set = BudgetPauseSet::default();
        let marker = set.park("ceo", None, "hi", "paused", 1_000, RedeemContext::default());

        let first = set.redeem_matching("ceo", &marker.id);
        assert_eq!(first, RedeemMatch::Reserved(marker.clone()));
        // A second attempt with the same id now finds nothing parked at all
        // (not "stale") — the first call already reserved it.
        assert_eq!(set.redeem_matching("ceo", &marker.id), RedeemMatch::Absent);
    }

    #[test]
    fn budget_pauses_are_scoped_per_company() {
        let acme = CompanyId::new("acme");
        let globex = CompanyId::new("globex");
        budget_pauses_for(&acme).park(
            "ceo",
            None,
            "acme's message",
            "paused",
            1_000,
            RedeemContext::default(),
        );

        assert!(
            budget_pauses_for(&globex).peek("ceo").is_none(),
            "a marker parked for one company must not leak into another's set"
        );
        assert!(budget_pauses_for(&acme).peek("ceo").is_some());
    }

    #[test]
    fn an_unrelated_agent_has_no_parked_marker() {
        let set = BudgetPauseSet::default();
        set.park("ceo", None, "hi", "paused", 1_000, RedeemContext::default());
        assert!(
            set.peek("engineer").is_none(),
            "parking for one agent must not be visible under another's key"
        );
    }

    /// Issue #1846 review (Codex #3865812419/#3865812423/#3865812432):
    /// `RedeemContext::from_events` reads the ORIGINAL operator message's
    /// parent/deliverable/mentions out of a cycle's event batch, skipping
    /// any non-`OperatorMessage` record ahead of it.
    #[test]
    fn redeem_context_reads_the_first_operator_message_in_a_batch() {
        use crate::ports::types::MentionTarget;

        let mention = Mention {
            target: MentionTarget::Agent {
                id: "researcher".to_string(),
            },
            text: "@researcher".to_string(),
            offset: 0,
            quiet: false,
        };
        let events = vec![
            (
                None,
                CompanyEvent::WorkspaceChanged {
                    node_id: "n-1".into(),
                    change: "updated".into(),
                },
            ),
            (
                None,
                CompanyEvent::OperatorMessage {
                    text: "ship it".into(),
                    by: None,
                    chat: Some("general".into()),
                    parent: Some(EventSeq::new(9)),
                    deliverable: Some(MessageIntent::Chat),
                    mentions: vec![mention.clone()],
                    attachments: Vec::new(),
                },
            ),
        ];

        let ctx = RedeemContext::from_events(&events);
        assert_eq!(ctx.parent, Some(EventSeq::new(9)));
        assert_eq!(ctx.deliverable, Some(MessageIntent::Chat));
        assert_eq!(ctx.mentions, vec![mention]);
    }

    #[test]
    fn redeem_context_defaults_when_no_operator_message_is_in_the_batch() {
        let events = vec![(
            None,
            CompanyEvent::WorkspaceChanged {
                node_id: "n-1".into(),
                change: "updated".into(),
            },
        )];
        assert_eq!(
            RedeemContext::from_events(&events),
            RedeemContext::default(),
            "a batch with no OperatorMessage carries nothing to replay"
        );
    }

    /// Issue #1846 review: the ambient scope round-trips exactly the shape
    /// `CHAT_ONLY_TURN` already proves for its own hint — set, read from
    /// inside, and gone once the scope's future finishes.
    #[tokio::test]
    async fn current_redeem_context_reads_the_ambient_scope_and_defaults_outside_it() {
        assert_eq!(
            current_redeem_context(),
            RedeemContext::default(),
            "outside any scope, the ambient context is the default"
        );

        let ctx = RedeemContext {
            parent: Some(EventSeq::new(3)),
            deliverable: Some(MessageIntent::Workflow),
            mentions: Vec::new(),
            text: None,
            attachments: Vec::new(),
        };
        let read_back = with_redeem_context(ctx.clone(), async { current_redeem_context() }).await;
        assert_eq!(
            read_back, ctx,
            "inside the scope, the ambient context is what was set"
        );

        assert_eq!(
            current_redeem_context(),
            RedeemContext::default(),
            "the scope does not leak past its own future"
        );
    }
}
