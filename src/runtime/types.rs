//! Runtime-local payloads: the outcome of running a cycle and a company's
//! status snapshot.

use serde::{Deserialize, Serialize};

use crate::ports::types::{
    ApprovalId, CompanyId, Effect, EventSeq, OutboundMessage, TemplateProvenance,
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
}

/// A compact status snapshot for a running company.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompanyStatus {
    /// The company id.
    pub id: CompanyId,
    /// The display name.
    pub name: String,
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
    pub at_millis: u64,
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
}
