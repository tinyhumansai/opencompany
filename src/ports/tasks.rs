//! The [`TaskStore`] port: the company's durable Kanban board.
//!
//! Tasks are the operator-visible work items the console's board renders (see
//! [`BOARD_COLUMNS`]). They are hand-curated state, not cycle working memory —
//! the brain's per-cycle task results live in
//! [`MemoryStore`](crate::ports::MemoryStore). Each record is keyed by a stable
//! id within the company.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::artifacts::ArtifactKind;
use crate::ports::runs::RunStatus;
use crate::ports::types::CompanyId;

// ---------------------------------------------------------------------------
// The board's column vocabulary (issue #205)
// ---------------------------------------------------------------------------
//
// Transcribed once here — the port every writer already depends on — rather
// than in the harness lifecycle seam, which is `#[cfg(feature = "openhuman")]`
// and therefore invisible to the REST write boundary and to the default build.
// `crate::harness::lifecycle` re-exports these so its `COLUMN_*` paths are
// unchanged, and `CompanyRuntime`'s dispatch edge reads `COLUMN_IN_PROGRESS`
// from here, so each literal exists in exactly one place.

/// Everything not started (issue #301): work nobody has picked up yet **and**
/// work a failed, cancelled or revised run returned. This is the board's
/// manual-entry column — the `+` button lives here alone — and what
/// `POST …/tasks` defaults to.
///
/// Before #301 the returned half lived in a separate `backlog` pool (issue
/// #206). Epic #183 §3 collapsed the two: the split encoded *why* a card had
/// not started — never picked vs bounced back — but that is provenance, not
/// position, and every return path already stamps its reason onto the card's
/// note (`review_note`, the dispatch error, `[operator] cancelled while in
/// flight`), which the board renders. See [`LEGACY_COLUMN_BACKLOG`] for how
/// stored cards migrate.
pub const COLUMN_TODO: &str = "todo";
/// Between intake and dispatch: the card is being turned into a plan.
///
/// **Live since issue #337.** Entering this column edge-fires one planning
/// pass — a single tool-less model call that writes a [`TaskPlan`] onto the
/// card ([`TaskRecord::plan`]) and then settles the card itself, three ways:
///
/// | Outcome | Landing |
/// | --- | --- |
/// | plan written, every hard prerequisite satisfied, assignee valid | [`COLUMN_IN_PROGRESS`] — the plan dispatches |
/// | plan written, a hard prerequisite missing (or no valid assignee) | [`COLUMN_TODO`], the gap named on the note |
/// | the pass itself failed (model error, timeout, unparseable output) | [`COLUMN_TODO`], the reason on the note, no plan |
///
/// So this column is **transient by construction**: a card resting here is a
/// pass in flight, or a host that died mid-pass — which the boot sweep
/// ([`sweep_stranded_planning`](crate::runtime::advance::sweep_stranded_planning))
/// returns to To-do at the next start.
///
/// The drag into it is **opt-in and it spends money**, which is the change #337
/// makes to the board's cost story: before this, `in_progress` was the only
/// column whose entry cost anything. Todo → In Progress still dispatches
/// unplanned, so nobody is forced through planning; but Todo → Planning is now
/// a spend gate of its own, and on success it hands straight on to the dispatch
/// gate without a second human step. See `docs/spec/runtime/planning.md`.
pub const COLUMN_PLANNING: &str = "planning";
/// The dispatch column: entering it hands the card to its assignee.
pub const COLUMN_IN_PROGRESS: &str = "in_progress";
/// Where a steered-to-pause run parks. Resume is a `column → in_progress` PATCH.
pub const COLUMN_PAUSED: &str = "paused";
/// Where a finished board-created card waits for its operator reviewer.
pub const COLUMN_IN_REVIEW: &str = "in_review";
/// The terminal column — nothing dispatches out of it.
pub const COLUMN_DONE: &str = "done";

/// Every column the board renders, in board order — the host's half of the
/// console's `TASK_COLUMNS` (`frontend/src/lib/tasks-sample.ts`).
///
/// A card's `column` is a plain string on the wire, so before #205 a typo'd or
/// invented column was persisted verbatim and then simply never rendered: the
/// card vanished from the board with no error, and — since only the exact
/// literal `in_progress` edge-fires a dispatch — a typo'd `in-progress` also
/// silently never ran. This list is what the write boundary checks against.
///
/// Issue #301 reshaped this list to epic #183 §3: `backlog` was removed and
/// `planning` added. `todo` absorbed both of `backlog`'s jobs — the unqueued
/// pool and the lifecycle's return landing — so a card that cannot be planned
/// goes back to `todo` with the reason on its note rather than into a second
/// not-started column. Issue #337 then made `planning` live: entering it runs
/// one planning pass and settles the card (see [`COLUMN_PLANNING`]).
pub const BOARD_COLUMNS: [&str; 6] = [
    COLUMN_TODO,
    COLUMN_PLANNING,
    COLUMN_IN_PROGRESS,
    COLUMN_PAUSED,
    COLUMN_IN_REVIEW,
    COLUMN_DONE,
];

/// The column id issue #206 used for the unqueued pool, removed by #301.
///
/// Kept only as the migration's left-hand side. Cards persisted before #301
/// carry this literal, which [`is_board_column`] now rejects — so without a
/// migration they would fail to render and vanish from the board, the exact
/// silent disappearance #205 exists to prevent. [`TaskRecord::column`]
/// normalizes it to [`COLUMN_TODO`] on **read**, so every stored card heals
/// lazily at its next load and persists the new literal at its next upsert.
///
/// Reads heal; writes do not. A client still *sending* `"backlog"` gets a
/// `400` from the REST write boundary naming the valid set, because the REST
/// DTOs deserialize `column` as a plain `String` and validate it separately —
/// legacy data recovers silently, a dead-column write fails loudly.
pub const LEGACY_COLUMN_BACKLOG: &str = "backlog";

/// Whether `column` names a column the board actually renders.
pub fn is_board_column(column: &str) -> bool {
    BOARD_COLUMNS.contains(&column)
}

/// Where a card lands when its attempt settles into `status` — the single
/// authority for the board's automatic edge (issue #337, epic #183 §4).
///
/// `None` means **do not move the card**: [`RunStatus::Pending`] and
/// [`RunStatus::Running`] are not settled at all, and a run still in flight has
/// no landing to write.
///
/// The mapping, and why each row is what it is:
///
/// | Settled status | Column | Why |
/// | --- | --- | --- |
/// | [`Succeeded`](RunStatus::Succeeded) | [`COLUMN_IN_REVIEW`] | a person accepts the result |
/// | [`WaitingApproval`](RunStatus::WaitingApproval) | [`COLUMN_IN_REVIEW`] | a person authorises the call |
/// | [`Paused`](RunStatus::Paused) | [`COLUMN_PAUSED`] | resumed, not approved |
/// | [`Failed`](RunStatus::Failed) | [`COLUMN_TODO`] | with the run's error on the card |
/// | [`Cancelled`](RunStatus::Cancelled) | [`COLUMN_TODO`] | with the cancellation reason on the card |
/// | [`Pending`](RunStatus::Pending) / [`Running`](RunStatus::Running) | — | not settled |
///
/// # `Succeeded` lands in review, **not** in Done
///
/// This is an operator decision (2026-08-05) and it deliberately supersedes the
/// `Succeeded → Done` row in epic #183 §4: **[`COLUMN_DONE`] is reached only by
/// a person**, through [`review_landing_column`](crate::harness::lifecycle::review_landing_column).
/// A clean success is a result somebody still has to accept, and auto-filing it
/// as done would make the terminal column mean "the model stopped talking"
/// rather than "we shipped this".
///
/// The consequence is that In Review carries **two** meanings — *approve this
/// call* and *accept this result* — which are not interchangeable. The card is
/// what disambiguates them: every settle stamps its own reason onto the note,
/// so an operator opening an In Review card reads whether they are authorising
/// an action or signing off finished work rather than having to guess.
///
/// # Why it lives on the port
///
/// Three writers need it and only one of them can see the harness:
/// `run_task`'s settle (feature-gated), the cycle's terminality backstop and
/// the boot reaper's card sweep (both ungated). `crate::harness::lifecycle` is
/// `#[cfg(feature = "openhuman")]`, so it re-exports this rather than owning
/// it — the same argument that moved [`BOARD_COLUMNS`] here in issue #205.
///
/// The match is **exhaustive over [`RunStatus`] with no wildcard**, so adding a
/// status is a compile error here first: a new run state must state where its
/// card goes rather than silently inheriting somebody else's answer.
pub fn column_for_settled_run(status: RunStatus) -> Option<&'static str> {
    match status {
        // A person acts next either way; the note says which act is wanted.
        RunStatus::Succeeded | RunStatus::WaitingApproval => Some(COLUMN_IN_REVIEW),
        // Waiting on something other than a person — resume is a plain
        // `column → in_progress` PATCH, which re-fires dispatch.
        RunStatus::Paused => Some(COLUMN_PAUSED),
        // Not reviewable work. Epic #183 §3: a card that cannot proceed returns
        // to To-do carrying the reason, never into a stuck column of its own.
        RunStatus::Failed | RunStatus::Cancelled => Some(COLUMN_TODO),
        // Still in flight. Nothing to land.
        RunStatus::Pending | RunStatus::Running => None,
    }
}

/// The human label for a column id (`in_progress` → `In progress`).
///
/// The board's ids are wire words; anything a person reads needs the label. The
/// console has carried these in `TASK_COLUMNS`
/// (`frontend/src/lib/tasks-sample.ts`) since #205 — this is the host's half,
/// added for the exported task record (issue #352), which is read by people who
/// have never seen the board and must not be shown `in_review`.
///
/// Like [`BOARD_COLUMNS`], the two lists are a hand-maintained mirror: a Rust
/// test cannot see the TS one, so a renamed label is a deliberate two-place
/// edit. An unknown id falls back to itself rather than to a guess, so a column
/// added on one side still prints something truthful.
///
/// `pub(crate)`: presentation is not part of the port's contract, and the only
/// consumer is the exported record. Widen it if a second surface needs it.
pub(crate) fn column_label(column: &str) -> &str {
    match column {
        COLUMN_TODO => "To-do",
        COLUMN_PLANNING => "Planning",
        COLUMN_IN_PROGRESS => "In progress",
        COLUMN_PAUSED => "Paused",
        COLUMN_IN_REVIEW => "In review",
        COLUMN_DONE => "Done",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// The per-task discussion (issue #335)
// ---------------------------------------------------------------------------

/// The longest discussion message the write boundary accepts, in **codepoints**
/// (issue #335).
///
/// A discussion post is a note on a card, not a document: the cap keeps one
/// paste from dominating a thread that is read back in full on every detail
/// poll, and it bounds what a single journal line can grow to. Text past it is
/// truncated rather than rejected, so a long paste still posts — the same
/// forgiving shape a steer's `instruction` takes
/// ([`MAX_REDIRECT_CHARS`](crate::company::steer::MAX_REDIRECT_CHARS)), and for
/// the same reason: losing the tail of an over-long note is a smaller failure
/// than losing the whole note to a `400`.
pub const MAX_DISCUSSION_CHARS: usize = 4000;

/// Truncates a discussion message to [`MAX_DISCUSSION_CHARS`] codepoints.
///
/// Counts characters, never bytes, so a message of multi-byte text is cut on a
/// character boundary instead of panicking on a split codepoint.
pub fn cap_discussion(text: &str) -> String {
    text.chars().take(MAX_DISCUSSION_CHARS).collect()
}

// ---------------------------------------------------------------------------
// The plan brief (issue #337, epic #183 §4)
// ---------------------------------------------------------------------------

/// What a prerequisite is a prerequisite *of* — the surface the host checks it
/// against.
///
/// The model proposes the kind; the **host** decides what to do with it. An
/// invented or misspelled kind deserializes to [`Self::Other`] rather than
/// failing the parse, and [`Other`](Self::Other) verifies to
/// [`PrereqStatus::Unknown`] — so a model that reaches for a surface this host
/// cannot inspect produces an honest "we could not check this", never a false
/// clear and never a lost plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PrereqKind {
    /// A `[[connection]]` the company declares (GitHub, Slack, …), checked
    /// against the same non-secret projection `GET …/connections` builds.
    Connection,
    /// A Composio-connected account, checked against the live toolkit list.
    Composio,
    /// A named MCP server, checked against the manifest ∪ runtime union.
    Mcp,
    /// A credential the company must hold for the work to be possible —
    /// checked for **presence only**, never read.
    Credential,
    /// A path in the company's shared workspace tree.
    File,
    /// A tool namespace the assignee must be granted, checked against the
    /// manifest allow-list and the approval policy.
    Permission,
    /// The teammate or desk the work is handed to.
    Assignee,
    /// A kind this host does not know how to check.
    #[serde(other)]
    Other,
}

impl PrereqKind {
    /// The wire word, for a note line a person reads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Composio => "composio",
            Self::Mcp => "mcp",
            Self::Credential => "credential",
            Self::File => "file",
            Self::Permission => "permission",
            Self::Assignee => "assignee",
            Self::Other => "other",
        }
    }
}

/// The host's verdict on one prerequisite the model claimed.
///
/// Only [`Missing`](Self::Missing) blocks. The other three ride along on the
/// brief: an approval-gated tool is a warning (the operator will be asked at
/// the moment it is used, which is the design), and an inventory the host could
/// not reach is an admission rather than a guess in either direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PrereqStatus {
    /// The host looked and found it. Does not block.
    Satisfied,
    /// The host looked and it is not there. **Blocks the dispatch.**
    Missing,
    /// Present, but the assignee's policy parks it for an operator when used.
    /// A warning, not a blocker.
    NeedsApproval,
    /// The host could not check — an inventory that errored or timed out, or a
    /// kind it does not know. Does not block; see the risk note in
    /// `docs/spec/runtime/planning.md`.
    Unknown,
}

impl PrereqStatus {
    /// Whether this verdict stops the card from dispatching.
    pub fn blocks(self) -> bool {
        matches!(self, Self::Missing)
    }

    /// The wire word, for a note line a person reads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::Missing => "missing",
            Self::NeedsApproval => "needsApproval",
            Self::Unknown => "unknown",
        }
    }
}

/// One thing the work needs before it can start: what the model claimed, and
/// what the host found when it looked.
///
/// The split is the whole point. A model asked "is GitHub connected?" will
/// answer from the prose in front of it; the host answers from the inventory.
/// So the model only ever fills [`kind`](Self::kind), [`name`](Self::name) and
/// the *why* half of [`note`](Self::note) — [`status`](Self::status) is stamped
/// by [`verify`](crate::harness::planning) against a real surface, and the note
/// is rewritten to say what an operator should actually do about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prerequisite {
    /// Which surface this is checked against.
    pub kind: PrereqKind,
    /// The thing itself: a provider slug, an MCP server name, a workspace
    /// path, a tool namespace, a teammate id.
    pub name: String,
    /// The host's verdict. Never supplied by the model — see the type docs.
    pub status: PrereqStatus,
    /// One line a person can act on: why the work needs it, and where to go if
    /// it is missing.
    pub note: String,
}

/// One step of the plan.
///
/// The estimates are the model's guesses and are labelled as such everywhere
/// they are rendered. **Nothing derives a budget from them** — the real caps
/// are the manifest's `budget_usd_daily` and the capability tier, both enforced
/// from live meter reads. An estimate that is wrong costs a person a moment of
/// recalibration; an estimate wired into a gate would silently mis-fund work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    /// A short imperative title.
    pub title: String,
    /// What doing it actually involves.
    pub detail: String,
    /// The model's guess at what this step costs, in USD. Advisory only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    /// The model's guess at how long this step takes. Advisory only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_minutes: Option<u32>,
}

/// The brief one planning pass produced for a card (issue #337).
///
/// # Why a field and not an artifact, and not note prose
///
/// Not an **artifact**: issue #244's rule is that artifacts are deliverables
/// *published from a run*, identified by `(task, source)`. A planning pass runs
/// no run — there is no attempt row, no agent turn, no workspace file — so an
/// artifact here would be a deliverable with no producer, and would show up in
/// the Artifacts tab as though the work had output.
///
/// Not **note prose**: the note is the card's append-only history and is read
/// as a transcript. Verdicts and estimates need structure the console can badge
/// and the next pass can overwrite. The note still gets one appended outcome
/// line per pass, so the history channel stays complete either way.
///
/// **Re-planning overwrites.** A second drag into Planning replaces this whole
/// value; the note keeps the trail of both. The alternative — a vector of
/// briefs — would make "the plan" ambiguous at exactly the moment the operator
/// wants an answer.
///
/// Additive on the wire ([`serde(default)`] + skip-if-none), the same shape
/// [`TaskRecord::origin_chat_id`] and [`TaskRecord::parent_task_id`] took, so
/// no stored board needs migrating on any of the three backends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPlan {
    /// What this task actually is, restated by the planner.
    pub description: String,
    /// How to do it, in order.
    pub steps: Vec<PlanStep>,
    /// What it needs first, with the host's verdict on each.
    pub prerequisites: Vec<Prerequisite>,
    /// What could go wrong.
    pub risks: Vec<String>,
    /// How a person will know it worked.
    pub verification: String,
    /// What is in scope, and explicitly what is not.
    pub scope: String,
    /// The teammate the planner would hand it to. Applied to the card **only**
    /// when the card had no assignee and this names a real one — a plan never
    /// reassigns work a person already assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_assignee: Option<String>,
    /// When the pass that produced this brief finished.
    pub planned_at_millis: u64,
}

impl TaskPlan {
    /// Every prerequisite whose verdict blocks the dispatch.
    pub fn blockers(&self) -> Vec<&Prerequisite> {
        self.prerequisites
            .iter()
            .filter(|p| p.status.blocks())
            .collect()
    }

    /// Whether the plan clears the card to dispatch.
    pub fn is_dispatchable(&self) -> bool {
        self.blockers().is_empty()
    }
}

/// Rewrites a stored column literal that no longer names a board column.
///
/// Today that is exactly one mapping: [`LEGACY_COLUMN_BACKLOG`] →
/// [`COLUMN_TODO`] (issue #301). Applied on deserialization of
/// [`TaskRecord::column`], which is the single choke point every persistence
/// backend and the export/import path all funnel through — sqlite and mongodb
/// both store the record as a `task_json` string and the fs bundle as a JSON
/// array, so all three parse through this one `impl Deserialize`.
fn migrate_column(column: String) -> String {
    if column == LEGACY_COLUMN_BACKLOG {
        COLUMN_TODO.to_string()
    } else {
        column
    }
}

/// Serde shim applying [`migrate_column`] to a stored `column`.
fn deserialize_column<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(migrate_column(String::deserialize(deserializer)?))
}

// ---------------------------------------------------------------------------
// What the card produced (issue #339, epic #183 §6)
// ---------------------------------------------------------------------------

/// What one attempt at a card produced — the card's link to its deliverable.
///
/// # Why the card carries this at all
///
/// Every ingredient already existed and nothing tied them to the card. Attempts
/// are first-class ([`RunRecord`](crate::ports::runs::RunRecord)); a published
/// file is a versioned [`ArtifactRecord`](crate::ports::artifacts::ArtifactRecord)
/// whose versions carry the producing
/// [`run_id`](crate::ports::artifacts::ArtifactVersion::run_id); the completion
/// event carries the artifact ids. But a *card* had no output field, so the end
/// of a task was a chat message: prose in a conversation, with no durable thing
/// to open, share, or hand to somebody else. This is that thing.
///
/// # `run_id` is unconditional, and that is the whole design
///
/// An output with **no** artifacts and **no** workflows is not an absence — it
/// is the *trace case*, and it is the common one. Plenty of tasks produce no
/// file (a message sent, a record updated, a question answered), and for those
/// the attempt's trace **is** the deliverable. Making `run_id` mandatory is what
/// stops "no artifact" degrading into "no link", which is the failure epic #183
/// §6 exists to close. It is also the fallback when an artifact is later
/// deleted: the pinned artifact link dangles, the trace does not.
///
/// # Which attempt, and why nothing has to choose
///
/// The latest **successful** one, by construction rather than by query: only a
/// settle that reached its success terminal writes this, and a later success
/// overwrites it wholesale. So a retried task links to the success, a failed
/// retry after a success never erases the link to that success, and earlier
/// attempts stay reachable through the card's own attempt list. No read-time
/// join decides it, so there is nothing for two readers to disagree about.
///
/// # One primary link, derived and never stored
///
/// The card shows a single link; the precedence is **artifact > workflow >
/// trace**, and anything beyond the first is reached through the task detail.
/// That primary is computed from this record at render time and deliberately
/// *not* persisted as a field, so it can never contradict the list it was
/// derived from. The console owns that derivation
/// (`frontend/src/api/tasks.ts`); this record owns the facts it derives from.
///
/// Additive on the wire, like [`TaskRecord::origin_chat_id`] and
/// [`TaskRecord::parent_task_id`]: every backend stores a card as a JSON blob
/// (`task_json` on sqlite, a document on MongoDB, a JSON array in the fs
/// bundle), so this needs **no migration** and a card written before it existed
/// loads with `None`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOutput {
    /// The attempt that produced it — always present, and the link of last
    /// resort (see the type's docs).
    pub run_id: String,
    /// That attempt's 1-based ordinal, for the operator-facing label
    /// (*"attempt 2"*).
    ///
    /// `None` when the run row could not be read at stamp time. The ordinal is
    /// a label, never an identity: `run_id` addresses the attempt either way, so
    /// a failed read costs a nicety and never a link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// Epoch-millis the stamp was written (the moment the attempt settled).
    pub at_millis: u64,
    /// Every artifact the attempt published, pinned at the version it wrote.
    ///
    /// Bounded by what the agent explicitly published — one entry per
    /// `publish_artifact` call, typically one to three. Empty is the trace case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<TaskOutputArtifact>,
    /// Every workflow the attempt ran or authored.
    ///
    /// Only ever non-empty for an orchestrator-desk card: `run_workflow` and
    /// `create_workflow` are orchestrator-only tools, so a card worked by any
    /// other teammate cannot produce one. That is a real limit of the
    /// correlation, not of the record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<TaskOutputWorkflow>,
}

/// One published deliverable, pinned to the exact revision this attempt wrote.
///
/// # Why the version is part of the link
///
/// Artifact versions are append-only and an operator edit is *a new version by
/// a different author* ([`ArtifactRecord`](crate::ports::artifacts::ArtifactRecord)),
/// so a `(artifact_id, version)` pair can never dangle and never silently start
/// meaning something else. A link to the record alone would quietly re-point at
/// whatever a human last wrote, which is exactly the case epic #183 §6 asks to
/// keep truthful: *a task whose output was later edited by a human still
/// resolves, and says so.* Pinning is what makes the "says so" possible — the
/// console can compare the pinned version against the latest and name the
/// difference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOutputArtifact {
    /// The artifact record's id.
    pub artifact_id: String,
    /// The 1-based revision this attempt wrote.
    pub version: u32,
    /// The artifact's display title, copied so the card can label its link
    /// without a second read on every board poll.
    pub title: String,
    /// What it holds, so the console picks a renderer without opening it.
    pub kind: ArtifactKind,
}

/// What an attempt did with a workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskOutputAction {
    /// It executed the graph.
    Ran,
    /// It authored and saved the graph.
    Created,
}

impl TaskOutputAction {
    /// The stable wire word, for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskOutputAction::Ran => "ran",
            TaskOutputAction::Created => "created",
        }
    }
}

/// A workflow the attempt ran or built.
///
/// # Deliberately **not** pinned to a graph revision
///
/// Unlike an artifact, a workflow graph mutates in place — there is no version
/// history to pin to, and inventing one would mean snapshotting graphs on every
/// edit, which this codebase does not do. So the link opens the workflow *as it
/// is now*, and when the attempt actually executed it, [`Self::run_id`] opens
/// the run overlay showing what really ran. That is the honest limit: a graph
/// edited after the fact will not match the run, and the run record is what
/// says what happened. Naming that beats hiding it behind a link that pretends
/// to be pinned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOutputWorkflow {
    /// The workflow's id (its `workflows/<id>.toml` stem).
    pub workflow_id: String,
    /// The workflow run this attempt started, when it ran one. `None` for a
    /// workflow the attempt only authored — nothing has executed yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Whether the attempt ran it or created it.
    pub action: TaskOutputAction,
}

/// One card on the company's task board.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    /// Stable id for the task within the company.
    pub id: String,
    /// The task's title.
    pub title: String,
    /// An optional longer note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The board column — one of [`BOARD_COLUMNS`].
    ///
    /// Read through [`migrate_column`], so a card persisted before issue #301
    /// under the removed `backlog` id loads as [`COLUMN_TODO`] instead of as a
    /// column nothing renders.
    #[serde(deserialize_with = "deserialize_column")]
    pub column: String,
    /// The priority (`low`, `medium`, `high`).
    pub priority: String,
    /// Which desk/teammate owns it.
    pub assignee: String,
    /// Epoch-millis timestamp of the last update.
    pub updated_at_millis: u64,
    /// The chat/desk thread the task was created from, when it came from a
    /// delegation (issue #151 §3.2).
    ///
    /// A dispatched card runs asynchronously, long after the turn that spawned
    /// it has answered, so the completion reply has no ambient thread to post
    /// onto — without this it can only be written into `note`, where the
    /// operator has to go looking for it. Stamped by `spawn_task` from the
    /// delegating turn's chat id.
    ///
    /// `None` for a card created straight on the board (no originating
    /// conversation) and for every card written before this field existed —
    /// both simply get no post-back, exactly as today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_chat_id: Option<String>,
    /// The task whose dispatch turn spawned this card (issue #185) — the
    /// parent half of the Task Detail screen's lineage.
    ///
    /// An agent running a dispatched card can itself delegate, opening a new
    /// To-do card through the same `spawn_task` path. That makes the board a
    /// forest, but nothing recorded the edge: `origin_chat_id` names the
    /// *conversation* a card came from, which is shared by every sibling
    /// spawned in that thread and is `None` entirely for a board-native card.
    /// Lineage needs the task-to-task edge, so it gets its own field.
    ///
    /// Read back as: `parent` is the card with this id; `children` are the
    /// cards whose `parent_task_id` is this card's id.
    ///
    /// `None` for a card created straight on the board, for one delegated from
    /// an ordinary chat turn (no task in scope), and for every card written
    /// before this field existed — all three are lineage roots, exactly as
    /// today. Additive on the wire like [`Self::origin_chat_id`], so no stored
    /// board needs migrating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    /// What the card's latest **successful** attempt produced (issue #339) —
    /// the link that turns a finished card into something you can open.
    ///
    /// Written by the dispatch settle and only when the attempt reached its
    /// success terminal; a failed or cancelled settle leaves the previous stamp
    /// alone, so a retry that fails never erases the link to the success before
    /// it. See [`TaskOutput`] for which attempt wins and why nothing has to
    /// choose at read time.
    ///
    /// `None` for a card that has never succeeded, for one moved to Done by
    /// hand, and for every card settled before this field existed. Those degrade
    /// to a link to the card itself rather than to a synthesized output —
    /// fabricating an attempt id for a card that never recorded one would be a
    /// lie about identity, which is the one thing the epic's link must not be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<TaskOutput>,
    /// The brief the last planning pass wrote for this card (issue #337).
    ///
    /// `None` for a card nobody has planned — which is every card until it is
    /// dragged into [`COLUMN_PLANNING`], and every card written before this
    /// field existed. Overwritten by a re-plan; see [`TaskPlan`] for why it is
    /// a structured field rather than an artifact or note prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<TaskPlan>,
}

/// Durable per-company task board. Company A's tasks MUST be invisible to
/// company B.
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// Lists every task, most-recently-updated first.
    async fn list(&self, company: &CompanyId) -> Result<Vec<TaskRecord>>;
    /// Inserts or replaces a task by id.
    async fn upsert(&self, company: &CompanyId, task: &TaskRecord) -> Result<()>;
    /// Deletes a task by id; returns whether a task was removed.
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}

#[cfg(test)]
mod test {
    use super::*;

    /// Pins the **Rust** list's ids and their order against a literal, so a
    /// reorder or a rename is a deliberate two-place edit rather than a
    /// side effect.
    ///
    /// It does **not** protect against drift from the console's mirror in
    /// `frontend/src/lib/tasks-sample.ts` — a Rust test cannot see the TS list,
    /// so a column added on one side and not the other keeps this green.
    /// Closing that gap means generating one list from the other (a build step
    /// this crate does not have, across a separate npm build), so for now the
    /// mirror is maintained by hand and the two lists are reviewed together.
    #[test]
    fn columns_are_ordered_and_unique() {
        assert_eq!(
            BOARD_COLUMNS,
            [
                "todo",
                "planning",
                "in_progress",
                "paused",
                "in_review",
                "done"
            ]
        );
        let mut sorted = BOARD_COLUMNS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            BOARD_COLUMNS.len(),
            "column ids must be unique"
        );
    }

    #[test]
    fn is_board_column_accepts_only_board_columns() {
        for column in BOARD_COLUMNS {
            assert!(is_board_column(column), "{column} is a board column");
        }
        // Issue #301: To-do is the one not-started column, Planning is new, and
        // the old Backlog pool is gone — a client still writing it gets a 400
        // rather than a card the board cannot render.
        assert!(is_board_column(COLUMN_TODO));
        assert!(is_board_column(COLUMN_PLANNING));
        assert!(!is_board_column(LEGACY_COLUMN_BACKLOG));
        // Near-misses a typo'd client might send.
        assert!(!is_board_column("to_do"));
        assert!(!is_board_column("To-do"));
        assert!(!is_board_column("inprogress"));
        assert!(!is_board_column(""));
    }

    /// Issue #352: every board column has a human label, and an unknown id
    /// prints itself rather than a guess. The labels are the mirror of the
    /// console's `TASK_COLUMNS`; asserting them against literals is what makes a
    /// rename a deliberate two-place edit.
    #[test]
    fn every_column_has_a_human_label() {
        assert_eq!(column_label(COLUMN_TODO), "To-do");
        assert_eq!(column_label(COLUMN_PLANNING), "Planning");
        assert_eq!(column_label(COLUMN_IN_PROGRESS), "In progress");
        assert_eq!(column_label(COLUMN_PAUSED), "Paused");
        assert_eq!(column_label(COLUMN_IN_REVIEW), "In review");
        assert_eq!(column_label(COLUMN_DONE), "Done");
        for column in BOARD_COLUMNS {
            let label = column_label(column);
            assert!(!label.is_empty());
            assert!(
                !label.contains('_'),
                "{column} still reads as a wire word: {label}"
            );
        }
        assert_eq!(column_label("something_new"), "something_new");
    }

    /// Issue #337: the whole automatic edge, pinned status by status.
    ///
    /// Every row is asserted against a literal rather than derived, because the
    /// table *is* the decision — a mapping that computed itself from some other
    /// property could drift without this failing.
    #[test]
    fn a_settled_run_lands_its_card_by_the_table() {
        assert_eq!(
            column_for_settled_run(RunStatus::Succeeded),
            Some(COLUMN_IN_REVIEW)
        );
        assert_eq!(
            column_for_settled_run(RunStatus::WaitingApproval),
            Some(COLUMN_IN_REVIEW)
        );
        assert_eq!(
            column_for_settled_run(RunStatus::Paused),
            Some(COLUMN_PAUSED)
        );
        assert_eq!(column_for_settled_run(RunStatus::Failed), Some(COLUMN_TODO));
        assert_eq!(
            column_for_settled_run(RunStatus::Cancelled),
            Some(COLUMN_TODO)
        );
        // Not settled — an in-flight attempt has no landing to write.
        assert_eq!(column_for_settled_run(RunStatus::Pending), None);
        assert_eq!(column_for_settled_run(RunStatus::Running), None);
    }

    /// The operator decision of 2026-08-05, pinned as its own test because it
    /// supersedes the `Succeeded → Done` row epic #183 §4 originally wrote.
    ///
    /// **Done is reached only by a person.** Nothing in the automatic edge may
    /// write [`COLUMN_DONE`]; the only route there is
    /// `review_landing_column(Approve)`. If a future change makes a settle land
    /// in Done, it fails here first.
    #[test]
    fn the_automatic_edge_never_writes_done() {
        assert_eq!(
            column_for_settled_run(RunStatus::Succeeded),
            Some(COLUMN_IN_REVIEW),
            "a clean success stops for a person; Done is not automatic"
        );
        for status in [
            RunStatus::Pending,
            RunStatus::Running,
            RunStatus::WaitingApproval,
            RunStatus::Paused,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            assert_ne!(
                column_for_settled_run(status),
                Some(COLUMN_DONE),
                "{status} must not auto-advance a card to Done"
            );
        }
    }

    /// Whatever the table says must be a column the board actually renders —
    /// otherwise a settle writes a card straight off the board, which is the
    /// silent disappearance [`BOARD_COLUMNS`] exists to prevent.
    #[test]
    fn every_landing_is_a_real_board_column() {
        for status in [
            RunStatus::Pending,
            RunStatus::Running,
            RunStatus::WaitingApproval,
            RunStatus::Paused,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            if let Some(column) = column_for_settled_run(status) {
                assert!(is_board_column(column), "{status} lands in '{column}'");
            }
        }
    }

    /// Issue #335: a long paste is truncated on a character boundary, never on
    /// a byte one — a message of multi-byte text must not panic the write path
    /// or persist a split codepoint. Anything within the cap passes through
    /// untouched.
    #[test]
    fn cap_discussion_is_codepoint_safe() {
        let long: String = "é".repeat(MAX_DISCUSSION_CHARS + 50);
        let capped = cap_discussion(&long);
        assert_eq!(capped.chars().count(), MAX_DISCUSSION_CHARS);
        // A clean multiple of 2 (é is 2 bytes) — no half-written character.
        assert_eq!(capped.len(), MAX_DISCUSSION_CHARS * 2);

        assert_eq!(cap_discussion("looks good to me"), "looks good to me");
        assert_eq!(cap_discussion(""), "");
    }

    // --- The plan brief (issue #337) ----------------------------------------

    fn prereq(kind: PrereqKind, status: PrereqStatus) -> Prerequisite {
        Prerequisite {
            kind,
            name: "github".to_string(),
            status,
            note: "the work opens a pull request".to_string(),
        }
    }

    fn plan_with(prerequisites: Vec<Prerequisite>) -> TaskPlan {
        TaskPlan {
            description: "Open a PR that adds the changelog entry".to_string(),
            steps: vec![PlanStep {
                title: "Draft the entry".to_string(),
                detail: "Write it against the released version".to_string(),
                estimated_cost_usd: Some(0.02),
                estimated_minutes: Some(5),
            }],
            prerequisites,
            risks: vec!["the release may not be tagged yet".to_string()],
            verification: "the PR exists and CI is green".to_string(),
            scope: "the changelog only; no code changes".to_string(),
            proposed_assignee: Some("maya".to_string()),
            planned_at_millis: 42,
        }
    }

    /// Only `missing` blocks. `needsApproval` and `unknown` ride on the brief
    /// as warnings — an approval-gated tool is asked about at the moment it is
    /// used, and an inventory the host could not reach is an admission, not a
    /// verdict in either direction.
    #[test]
    fn only_a_missing_prerequisite_blocks_the_dispatch() {
        assert!(PrereqStatus::Missing.blocks());
        assert!(!PrereqStatus::Satisfied.blocks());
        assert!(!PrereqStatus::NeedsApproval.blocks());
        assert!(!PrereqStatus::Unknown.blocks());

        let clear = plan_with(vec![
            prereq(PrereqKind::Connection, PrereqStatus::Satisfied),
            prereq(PrereqKind::Permission, PrereqStatus::NeedsApproval),
            prereq(PrereqKind::Composio, PrereqStatus::Unknown),
        ]);
        assert!(clear.is_dispatchable());
        assert!(clear.blockers().is_empty());

        let blocked = plan_with(vec![
            prereq(PrereqKind::Connection, PrereqStatus::Satisfied),
            prereq(PrereqKind::Mcp, PrereqStatus::Missing),
        ]);
        assert!(!blocked.is_dispatchable());
        assert_eq!(blocked.blockers().len(), 1);
        assert_eq!(blocked.blockers()[0].kind, PrereqKind::Mcp);

        // A plan claiming nothing is dispatchable — "needs nothing" is a
        // legitimate answer, not a suspicious one.
        assert!(plan_with(Vec::new()).is_dispatchable());
    }

    /// A kind this host cannot check must not fail the parse and must not read
    /// as satisfied. It deserializes to `Other`, which the verifier stamps
    /// `unknown` — the model gets to be wrong without costing us the plan.
    #[test]
    fn an_unknown_prerequisite_kind_parses_as_other() {
        let raw = r#"{"kind":"quantum_flux","name":"x","status":"unknown","note":"n"}"#;
        let parsed: Prerequisite = serde_json::from_str(raw).expect("an odd kind still parses");
        assert_eq!(parsed.kind, PrereqKind::Other);
        assert_eq!(parsed.status, PrereqStatus::Unknown);

        // Every known kind still round-trips to its own variant.
        for (wire, kind) in [
            ("connection", PrereqKind::Connection),
            ("composio", PrereqKind::Composio),
            ("mcp", PrereqKind::Mcp),
            ("credential", PrereqKind::Credential),
            ("file", PrereqKind::File),
            ("permission", PrereqKind::Permission),
            ("assignee", PrereqKind::Assignee),
        ] {
            let raw = format!(r#"{{"kind":"{wire}","name":"x","status":"missing","note":"n"}}"#);
            let parsed: Prerequisite = serde_json::from_str(&raw).expect("known kind");
            assert_eq!(parsed.kind, kind);
            assert_eq!(parsed.kind.as_str(), wire);
        }
    }

    /// The additive-wire contract: a card persisted before #337 loads with no
    /// plan, and a card that has never been planned serializes byte-identically
    /// to the pre-#337 shape. This is what makes "no migration on any of the
    /// three backends" true rather than hoped for.
    #[test]
    fn the_plan_field_is_additive_on_the_wire() {
        let legacy = r#"{
            "id": "t-1",
            "title": "Unplanned work",
            "column": "todo",
            "priority": "medium",
            "assignee": "maya",
            "updatedAtMillis": 7
        }"#;
        let card: TaskRecord = serde_json::from_str(legacy).expect("a pre-#337 card parses");
        assert!(card.plan.is_none());

        // Matched on the key, not the substring: the fixture's title contains
        // the word "unplanned", and a looser check passes for the wrong reason.
        let round_tripped = serde_json::to_string(&card).unwrap();
        assert!(
            !round_tripped.contains("\"plan\":"),
            "an unplanned card must not grow a key: {round_tripped}"
        );

        // And a planned card round-trips its whole brief, verdicts included.
        let planned = TaskRecord {
            plan: Some(plan_with(vec![prereq(
                PrereqKind::Connection,
                PrereqStatus::Missing,
            )])),
            ..card
        };
        let json = serde_json::to_string(&planned).unwrap();
        assert!(json.contains("\"status\":\"missing\""), "{json}");
        assert!(json.contains("\"estimatedCostUsd\":0.02"), "{json}");
        let back: TaskRecord = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back, planned);
    }

    /// Issue #301's whole migration, at the seam every persistence backend
    /// shares. sqlite and mongodb store a card as a `task_json` string and the
    /// fs bundle as a JSON array, so all three — plus export/import — parse
    /// through this one `Deserialize`. A stored `backlog` card therefore heals
    /// on read instead of failing `is_board_column` and vanishing from the
    /// board.
    #[test]
    fn a_stored_backlog_card_reads_back_in_todo() {
        // A raw blob in exactly the shape a pre-#301 build persisted.
        let legacy = r#"{
            "id": "t-1",
            "title": "Bounced work",
            "note": "[operator] cancelled while in flight",
            "column": "backlog",
            "priority": "medium",
            "assignee": "maya",
            "updatedAtMillis": 7
        }"#;
        let migrated: TaskRecord = serde_json::from_str(legacy).expect("legacy card parses");
        assert_eq!(migrated.column, COLUMN_TODO);
        assert!(
            is_board_column(&migrated.column),
            "a migrated card must render on the board"
        );
        // The reason the collapse is lossless: the note survives untouched, so
        // "bounced back" is still readable on the card.
        assert_eq!(
            migrated.note.as_deref(),
            Some("[operator] cancelled while in flight")
        );

        // The next upsert persists the new literal — nothing re-writes it back.
        let round_tripped = serde_json::to_string(&migrated).unwrap();
        assert!(
            round_tripped.contains("\"column\":\"todo\""),
            "{round_tripped}"
        );

        // Migration is exactly one mapping; every live column is passed through
        // untouched, so a future column cannot be silently rewritten.
        for column in BOARD_COLUMNS {
            assert_eq!(migrate_column(column.to_string()), column);
        }
    }

    fn plain_card() -> TaskRecord {
        TaskRecord {
            id: "t-1".to_string(),
            title: "Draft the spec".to_string(),
            note: None,
            column: COLUMN_IN_REVIEW.to_string(),
            priority: "medium".to_string(),
            assignee: "maya".to_string(),
            updated_at_millis: 7,
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            // #339's baseline fixture stays baseline: it exists to prove the
            // output stamp round-trips against a card carrying nothing else.
            plan: None,
        }
    }

    /// Issue #339: the whole stamp round-trips as camelCase, and — the part
    /// that matters — a card written before it existed still loads, with
    /// `None`. All three backends persist a card as an opaque JSON blob, so a
    /// `#[serde(default)]` field is the entire migration.
    #[test]
    fn an_output_stamp_round_trips_and_is_absent_on_a_legacy_card() {
        let mut card = plain_card();
        card.output = Some(TaskOutput {
            run_id: "run-2".to_string(),
            attempt: Some(2),
            at_millis: 99,
            artifacts: vec![TaskOutputArtifact {
                artifact_id: "a-1".to_string(),
                version: 3,
                title: "Launch spec".to_string(),
                kind: ArtifactKind::Markdown,
            }],
            workflows: vec![TaskOutputWorkflow {
                workflow_id: "digest".to_string(),
                run_id: Some("wf-run-1".to_string()),
                action: TaskOutputAction::Ran,
            }],
        });

        let json = serde_json::to_string(&card).expect("serialize");
        assert!(json.contains(r#""runId":"run-2""#), "{json}");
        assert!(json.contains(r#""artifactId":"a-1""#), "{json}");
        assert!(json.contains(r#""action":"ran""#), "{json}");
        assert_eq!(card, serde_json::from_str(&json).expect("round trip"));

        // Exactly the blob a pre-#339 build persisted: no `output` key at all.
        let legacy = r#"{
            "id": "t-1",
            "title": "Draft the spec",
            "column": "done",
            "priority": "medium",
            "assignee": "maya",
            "updatedAtMillis": 7
        }"#;
        let loaded: TaskRecord = serde_json::from_str(legacy).expect("a legacy card parses");
        assert_eq!(
            loaded.output, None,
            "a card that never recorded an attempt must not be given a synthesized one"
        );
        // And an unstamped card carries no empty scaffolding on the wire.
        let round_tripped = serde_json::to_string(&loaded).expect("serialize");
        assert!(!round_tripped.contains("output"), "{round_tripped}");
    }

    /// The trace case, pinned as its own test because it is the acceptance
    /// criterion most easily lost: *"every card has a link including tasks that
    /// produced no file."* An output with neither artifacts nor workflows is a
    /// complete, valid stamp — the attempt is the deliverable — so `runId` must
    /// survive a round trip on its own, with both lists omitted entirely.
    #[test]
    fn a_task_that_produced_no_file_still_carries_a_link() {
        let mut card = plain_card();
        card.output = Some(TaskOutput {
            run_id: "run-1".to_string(),
            attempt: Some(1),
            at_millis: 5,
            artifacts: Vec::new(),
            workflows: Vec::new(),
        });

        let json = serde_json::to_string(&card).expect("serialize");
        assert!(json.contains(r#""runId":"run-1""#), "{json}");
        assert!(!json.contains("artifacts"), "{json}");
        assert!(!json.contains("workflows"), "{json}");

        let back: TaskRecord = serde_json::from_str(&json).expect("round trip");
        let output = back.output.expect("the stamp survives with no deliverable");
        assert_eq!(output.run_id, "run-1");
        assert!(output.artifacts.is_empty());
        assert!(output.workflows.is_empty());
    }

    /// The attempt ordinal is a label, not an identity: a stamp written when
    /// the run row could not be read still addresses its attempt.
    #[test]
    fn an_unknown_attempt_ordinal_still_leaves_an_addressable_link() {
        let output = TaskOutput {
            run_id: "run-9".to_string(),
            attempt: None,
            at_millis: 5,
            artifacts: Vec::new(),
            workflows: Vec::new(),
        };
        let json = serde_json::to_string(&output).expect("serialize");
        assert!(!json.contains("attempt"), "{json}");
        let back: TaskOutput = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back.run_id, "run-9");
        assert_eq!(back.attempt, None);
    }
}
