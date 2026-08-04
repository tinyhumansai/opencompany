//! Runtime-local payloads: the outcome of running a cycle and a company's
//! status snapshot.

use serde::{Deserialize, Serialize};

use crate::ports::types::{
    ApprovalId, CompanyId, Effect, EventSeq, OutboundMessage, TemplateProvenance,
};

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
}
