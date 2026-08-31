//! Runtime-local payloads: the outcome of running a cycle and a company's
//! status snapshot.

use serde::{Deserialize, Serialize};

use crate::ports::types::{
    ApprovalId, CompanyId, Effect, EffectGroup, EventSeq, OutboundMessage, TemplateProvenance,
};

/// Which board task an approval was parked for (issue #333).
///
/// Two arms rather than an `Option<String>` because "no card is behind this
/// one" is a recorded fact, not a missing one. A host from #333 onward always
/// writes one of these; an absent link (`Option<TaskLink>::None`) means only
/// that the journal line predates the field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "link", rename_all = "snake_case")]
pub enum TaskLink {
    /// Parked inside a board task's dispatch cycle — that card owns it.
    Task {
        /// The owning board task's id.
        id: String,
    },
    /// Parked with no board task behind it, recorded as such.
    Unlinked,
}

impl TaskLink {
    /// The owning task's id, or `None` for [`Unlinked`](Self::Unlinked).
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::Task { id } => Some(id.as_str()),
            Self::Unlinked => None,
        }
    }

    /// Builds a link from an optional task id — `None` becoming an explicit
    /// [`Unlinked`](Self::Unlinked) rather than a missing link.
    pub fn from_task_id(task_id: Option<&str>) -> Self {
        match task_id {
            Some(id) => Self::Task { id: id.to_string() },
            None => Self::Unlinked,
        }
    }
}

/// The outcome of one cycle: what the brain said, what effects ran or parked,
/// and where the event log now stands.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CycleReport {
    /// The cycle's unique id.
    pub cycle_id: String,
    /// Channel responses the brain produced.
    pub responses: Vec<OutboundMessage>,
    /// Effects that were executed this cycle.
    pub executed_effects: Vec<Effect>,
    /// Approvals parked this cycle, awaiting the operator.
    pub parked: Vec<ApprovalId>,
    /// The sequence of the last event appended this cycle, if any.
    pub persisted_seq: Option<EventSeq>,
    /// The sequence each of this cycle's **input** events was journaled under,
    /// in the order they were handed in (issue #364).
    ///
    /// `persisted_seq` next door is the *last* append and cannot answer this:
    /// by the time a cycle returns it names whatever the cycle wrote last, not
    /// the operator message that started it. Without a per-input seq the chat
    /// route has no durable id for the message the operator just sent, so a
    /// thread reply or a reaction made against it names a browser-minted
    /// counter that nothing else can resolve.
    ///
    /// Read off the append loop, which already computes it — the alternative
    /// (journaling the message in the route before calling the runtime) would
    /// either double-journal it or move the append out of the one place that
    /// orders it against the rest of the cycle.
    ///
    /// Empty on a synthetic report (an already-resolved approval) and on a
    /// report deserialized from before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_seqs: Vec<EventSeq>,
}

/// A compact status snapshot for a running company.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompanyStatus {
    /// The company id.
    pub id: CompanyId,
    /// The display name.
    pub name: String,
    /// The operator-set company logo, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    /// Lifecycle state, e.g. `running`, `paused`, `archived`.
    pub lifecycle: String,
    /// The number of approvals currently awaiting the operator.
    pub pending_approvals: usize,
    /// The source-template provenance recorded at launch — the stable template
    /// id (directory slug) and, when known, its version. Absent for a company
    /// provisioned from a raw manifest rather than a template. Drives the
    /// console's read-only "Launched from template" line (issue #85).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_provenance: Option<TemplateProvenance>,
    /// Whether the governance kill switch is engaged (issue #86): new effects
    /// outside `EffectGroup::Other` are being denied.
    ///
    /// Orthogonal to `lifecycle` — a company in emergency stop still reports
    /// `running`, because chat still works. A console that only reads
    /// `lifecycle` would show it as perfectly healthy, which is exactly the
    /// reading this field exists to prevent.
    ///
    /// `#[serde(default)]` (to `false`) keeps a status payload produced before
    /// the kill switch existed deserializing unchanged.
    #[serde(default)]
    pub emergency_paused: bool,
}

/// A parked approval as surfaced to the operator.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalSummary {
    /// The approval's id.
    pub id: ApprovalId,
    /// The parked effect's dotted kind.
    pub kind: String,
    /// The USD amount involved, if any.
    pub amount_usd: Option<f64>,
    /// Epoch-millis the effect was parked.
    ///
    /// Stamped by `CycleRunner::park` in the same turn that composed the
    /// effect's arguments, so it dates the PAYLOAD, not the queue: it is read
    /// back off the journal record on replay rather than re-stamped, so a host
    /// restart does not reset it to boot time. That is what lets the console
    /// say how old the content is rather than how long the card has sat
    /// (issue #1024).
    pub at_millis: u64,
    /// Epoch-millis this approval default-denies if nobody decides it
    /// (issue #971) — `at_millis` plus the gate's TTL.
    ///
    /// **Projected, not a second source of truth.** The deadline is the gate's
    /// (`[policy].approval_ttl_hours`, defaulting to 24 hours), and the gate
    /// re-checks it on every resolve; this is that same number said out loud so
    /// a card can show it. Computing it a second way in the console — or on the
    /// GraphQL side — would be a deadline the host does not enforce, which is
    /// worse than no deadline at all: an operator would read "in 3h", act on
    /// it, and be refused.
    ///
    /// It is the honest half of shortening the deadline. An approval that
    /// vanishes is only acceptable if the card said when it would, so nothing
    /// disappears unannounced.
    ///
    /// Omitted when absent, the additive pattern the fields below follow: an
    /// old console ignores the key and a new one reads its absence as "this
    /// host does not report deadlines" and renders the card exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_millis: Option<u64>,
    /// The consequence group of the parked effect (issue #1024).
    ///
    /// Copied off [`Effect::group`], which the harness derives from the tool
    /// AND its arguments — so a `composio_execute` carrying `GMAIL_SEND_EMAIL`
    /// arrives here as [`EffectGroup::Send`], not as the catch-all its tool
    /// name alone would suggest.
    ///
    /// The console needs it to tell an effect that leaves the company from one
    /// that does not, and it cannot be derived client-side: for a harness tool
    /// call `kind` is the TOOL NAME, so a console keying on `kind` would miss
    /// exactly the outbound sends this exists to mark. Sending the host's own
    /// classification keeps that judgement in one place.
    pub group: EffectGroup,
    /// Which board task this approval was parked for (issue #333).
    ///
    /// Three states, and the Task Detail read depends on telling them apart:
    /// [`TaskLink::Task`] is owned by that card, [`TaskLink::Unlinked`] is owned
    /// by no card (a workflow delivery, an operator-chat turn, a scheduler
    /// tick), and `None` means the park predates the field — the only case that
    /// still falls back to the run-window heuristic. Omitted when absent, so a
    /// pre-#333 approval serializes as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskLink>,
    /// The roster teammate whose blocked tool call this approval was parked for
    /// (issue #372), mirroring [`Effect::agent`](crate::ports::types::Effect::agent).
    ///
    /// `Some(id)` is exactly "projected from a harness tool call" — the console
    /// renders "Asked by <name>". `None` is a *native* effect the runtime
    /// performs itself, or a park journaled before #243 stamped the field, and
    /// the card names no asker rather than inventing one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// What the effect will actually do, as an operator-readable copy of its
    /// payload (issue #372) — the tool-call arguments for a harness effect.
    ///
    /// Redacted and bounded host-side by
    /// [`display_payload`](crate::runtime::approval_display::display_payload),
    /// so no credential crosses the wire and no unbounded blob reaches a
    /// browser. `None` when the effect carries no arguments.
    ///
    /// Both new fields are omitted when absent, which is what keeps the wire
    /// additive: an old console ignores the unknown keys, and a new console
    /// treats their absence as "old host" and renders the card exactly as it
    /// did before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// The chat thread this approval was raised in (issue #379) — a desk id for
    /// a channel, a roster agent id for a direct message.
    ///
    /// The key that lets the console draw the request as a card *inside* that
    /// conversation. Not derivable from [`agent`](Self::agent): a desk channel
    /// and a direct message to that desk's lead are answered by the same
    /// teammate, so placing the card by asker would raise one conversation's
    /// request inside the other.
    ///
    /// `None` — and omitted from the wire — for an approval with no
    /// conversation behind it (a workflow delivery, a scheduler tick) and for
    /// every park journaled before this field existed. Both mean the same
    /// thing to a reader: no channel owns it, so it appears on the Approvals
    /// page and in no thread, exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// The **workflow run** waiting on this approval (issue #880), when one is.
    ///
    /// The other direction of #880's fix: [`WorkflowRun::approvals`](crate::ports::WorkflowRun)
    /// lets a run say what it parked; this lets a card say which run parked it.
    /// Without it the Approvals page shows fifteen `publish_artifact` cards
    /// with nothing tying any of them to the three runs that opened them.
    ///
    /// # Why this is not simply `effect.run_id`
    ///
    /// [`Effect::run_id`](crate::ports::types::Effect::run_id) is **overloaded**.
    /// It is documented as issue #242's *task-attempt* id — a
    /// [`RunRecord`](crate::ports::runs::RunRecord), stamped at the dispatch
    /// boundary — and the workflow path also writes a *workflow* run id into it
    /// (`workflows::caps::park_gated_calls` and
    /// `runtime::workflow_resume::gate_effect`, the latter saying so in its own
    /// comment). Two different id spaces in one field, and
    /// [`generate_id`](crate::ports::ids::generate_id) is only process-locally
    /// unique, so the ids cannot be told apart by inspection.
    ///
    /// The discriminator is the *park site*, which is recorded: a task attempt
    /// parks inside its dispatch cycle and is linked
    /// [`TaskLink::Task`](crate::runtime::journal::TaskLink), while every
    /// workflow park goes through
    /// [`DeliveryParking::park_and_journal`](crate::workflows::DeliveryParking)
    /// and is recorded explicitly [`Unlinked`](TaskLink::Unlinked) (#333). So
    /// "a run id on an approval that belongs to no card" is a workflow run, by
    /// construction rather than by guess — and a chat turn, which is also
    /// unlinked, stamps no run id at all.
    ///
    /// Omitted when absent, the additive pattern the two fields above follow: an
    /// old console ignores the key and a new one reads its absence as "no run
    /// behind this card", which is the truth for every chat and scheduler park.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    /// Which workflow the parked [`workflow.approve`] gate is asking about
    /// (issue #395), when the effect is one.
    ///
    /// The console's run address needs **both** halves — a run id alone cannot
    /// name a page, so this sits beside [`workflow_run_id`](Self::workflow_run_id)
    /// as the second half of the join.
    ///
    /// # Why this is a top-level field and not read from the payload
    ///
    /// The id also appears in the parked effect's payload (the `workflow_id`
    /// key), but [`payload`](Self::payload) is a redacted *rendering*
    /// (`approval_display`), and role redaction (issue #618) strips it from a
    /// member reader entirely. [`workflow_run_id`](Self::workflow_run_id)
    /// survives that redaction, and this must too, or the member holding up a
    /// stalled workflow would lose the one address that says where it is —
    /// exactly the stalled-work visibility issue #468 exists to protect.
    ///
    /// Projected from the raw parked effect
    /// ([`gate_workflow_id`](crate::runtime::workflow_resume::gate_workflow_id)),
    /// never from the display payload, for the same reason the projection's own
    /// comment gives: a fact must be read as a fact, not as a rendering that
    /// redaction rules could silently change.
    ///
    /// Absent on every non-gate approval (a chat turn, a scheduler tick) and on
    /// a tool call parked *by* a workflow — only native `workflow.approve`
    /// effects carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    /// Whether the operator may grant this tool **broadly** — one standing
    /// permission covering any arguments until a deadline (issue #374).
    ///
    /// `true` exactly when the effect came from a harness tool call *and* the
    /// tool cannot reach further than a standing permission can honestly
    /// describe — [`Effect::may_be_granted_standing`](crate::ports::types::Effect::may_be_granted_standing),
    /// issue #444. The console renders the scope control only then, so the
    /// operator is never offered a choice the host would refuse.
    ///
    /// It is per-*card*, not per-tool: the same `composio_execute` shows the
    /// control when it is listing a repository's pull requests and not when it
    /// is sending mail, because the effect carries the action that was called.
    ///
    /// **This flag is UX, not enforcement.** The host re-checks the same rule
    /// when the resolve arrives, and answers 400 — a console that ignored this
    /// field, or a hand-rolled request, gets no further than one that respects
    /// it.
    ///
    /// Skipped when `false`, which is the common case, so a card that cannot be
    /// granted broadly serializes exactly as it did before this field existed —
    /// and an old console, which reads no such field, degrades to today's
    /// approve-once behaviour by construction.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub broadly_grantable: bool,
    /// Whether this tool may be denied standing. Unlike a grant, a refusal is
    /// safe for every agent or workflow tool because it can only narrow access.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub broadly_deniable: bool,
    /// Whether [`payload`](Self::payload) and [`amount_usd`](Self::amount_usd)
    /// were withheld from *this reader* because of their role (issue #618).
    ///
    /// Set at the edge by
    /// [`approval_visibility`](crate::server::approval_visibility), never by
    /// the projection — the runtime does not know who is asking, and this is
    /// deliberately the only place that does.
    ///
    /// **The reason this flag exists rather than just blanking the fields:**
    /// `payload: None` already means "the effect carries no arguments". Without
    /// a separate signal, a withheld payment and a no-argument tool call are
    /// the same bytes on the wire, and a console cannot tell "there is nothing
    /// to show" from "you may not see it". The first renders as an ordinary
    /// empty card; the second has to say *hidden by your role*, or a Member is
    /// quietly misled about what they are looking at.
    ///
    /// Skipped when `false`, so an admin's response — and every response
    /// produced before this field existed — serializes byte-identically to
    /// before, and a console that reads no such field is unaffected.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub contents_hidden: bool,
    /// Which turn's gated calls this one belongs to (issue #842) — an opaque
    /// grouping key shared by every approval a single agent turn parked.
    ///
    /// **Presentation, not a new unit of truth.** One research turn that
    /// reaches three sites parks three approvals, and each stays exactly what
    /// it was: its own record, its own decision, its own host-scoped grant on
    /// approve (issue #739). This field only says that they were asked for
    /// together, so the conversation can ask once — "three sites" with the
    /// hosts listed — instead of interrupting three times. The Approvals page
    /// deliberately keeps rendering one row per approval, matching how
    /// `Standing permissions` lists one revocable row per grant.
    ///
    /// The value is the parking turn key issue #469 already journals, so the
    /// batch a card consolidates is by construction the same batch the runtime
    /// continues exactly once. Nothing else may be inferred from it: it is an
    /// opaque id, not an ordering, a count, or an address.
    ///
    /// `None` — and omitted from the wire — for an approval raised outside a
    /// cycle (a workflow node, a scheduler tick) and for every park journaled
    /// before #469. A console groups those alone, which is the pre-#842
    /// rendering, so an old host and a new one both produce a card an operator
    /// can decide.
    ///
    /// Survives role redaction on purpose:
    /// [`approval_visibility`](crate::server::approval_visibility) withholds
    /// *contents*, and which requests arrived together is not contents. A
    /// Member sees a batch of three withheld cards rather than three unrelated
    /// ones, which is less information than an admin gets and still the truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::CompanyStatus;

    /// **Replay compatibility (issue #86).** A status payload serialized before
    /// the kill switch existed has no `emergency_paused` key, and must read as
    /// `false` — the company is not stopped — so an old snapshot or a synthetic
    /// payload keeps working against a new host. This is the `#[serde(default)]`
    /// contract on the field, asserted directly rather than only through the
    /// live API responses.
    #[test]
    fn a_status_without_emergency_paused_deserializes_as_not_stopped() {
        let payload = serde_json::json!({
            "id": "acme",
            "name": "Acme",
            "lifecycle": "running",
            "pending_approvals": 3,
        });
        let status: CompanyStatus =
            serde_json::from_value(payload).expect("a pre-stop status payload still deserializes");
        assert!(!status.emergency_paused);
    }
}
