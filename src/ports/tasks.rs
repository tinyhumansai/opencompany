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
/// # What rests here, now that the prompt box does not (issue #576)
///
/// A card opened from the prompt box by a person goes straight to
/// [`COLUMN_PLANNING`], so the busiest intake path no longer stops here. To-do
/// is not thereby empty, and it is not a legacy column — it is **where work
/// waits for a person to decide it should start**, which is exactly the set of
/// cards nobody has yet spent anything on:
///
/// * **Entered by hand** — the `+` button, and `POST …/tasks`. Jotting a card
///   down must not buy a planning call, so intake here stays inert.
/// * **Opened by something that is not a person** — the chat route also accepts
///   machine credentials, and a self-promoting agent card would buy a pass whose
///   own output can open more cards. See `docs/spec/runtime/planning.md`.
/// * **Returned** — a planning pass that failed or found a hard prerequisite
///   missing lands the card back here with its reason, as does a rejected
///   workflow proposal and a cancelled run. This is the largest half in
///   practice, and it is why the column has to keep a clear meaning: a returned
///   card is waiting for a person to read the reason and decide.
///
/// So the column's meaning is unchanged in kind — not-started work — and
/// narrowed in membership. The drag out of it is still the human "yes".
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

/// Every column the board renders, in board order.
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
///
/// **Derived, no longer declared.** It used to be a second literal list beside
/// [`crate::ledger::board::COLUMNS`], with the console keeping a third; the
/// board's own `TASK_COLUMNS` comment admitted what that cost — *"a Rust test
/// cannot see the TS list, so a column added on one side and not the other
/// keeps this green."* Now the table is the declaration, this is a `const fn`
/// projection of it, and the console reads its labels off the `tasks` ledger.
/// One edit adds a column everywhere.
pub const BOARD_COLUMNS: [&str; crate::ledger::board::COLUMNS.len()] = crate::ledger::board::ids();

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
/// | [`WaitingApproval`](RunStatus::WaitingApproval) | [`COLUMN_PAUSED`] | the run stopped short; nothing to accept yet |
/// | [`Paused`](RunStatus::Paused) | [`COLUMN_PAUSED`] | resumed, not approved |
/// | [`Failed`](RunStatus::Failed) | [`COLUMN_TODO`] | with the run's error on the card |
/// | [`Cancelled`](RunStatus::Cancelled) | [`COLUMN_TODO`] | with the cancellation reason on the card |
/// | [`Declined`](RunStatus::Declined) | [`COLUMN_TODO`] | a by-design decline (#1809); reason on the card, now a one-off |
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
/// # `WaitingApproval` lands in Paused, **not** in review (issue #465)
///
/// This row used to share [`COLUMN_IN_REVIEW`] with `Succeeded`, on the argument
/// that "a person acts next either way; the note says which act is wanted". That
/// made In Review carry two non-interchangeable meanings — *approve this call*
/// and *accept this result* — and left the operator to tell them apart from the
/// note.
///
/// It is not a labelling problem. **[`COLUMN_IN_REVIEW`] is defined by the
/// verdict that consumes it**: `review_landing_column(Approve)` writes
/// [`COLUMN_DONE`], and it is the only route there. So a run that stopped at an
/// unauthorised call sat one click away from being filed as finished — the exact
/// state issue #337 set out to remove when it stopped a parked run auto-landing
/// in Done. #337 closed the automatic route and left the manual one open.
///
/// A run parked on an approval has not produced a reviewable result: it stopped
/// early, and in the worst case (its *first* call parked) it produced nothing at
/// all while the board announced work to check. So the parked case leaves the
/// review columns entirely and lands in [`COLUMN_PAUSED`] — the board's "stopped,
/// not finished" column, and the one column the console gives a Resume control,
/// which is the gesture that actually puts the work back in flight once the call
/// is authorised.
///
/// **This narrows epic #183 decision 2** ("Paused vs In review is decided by who
/// unblocks it — a person must act → In review"), which is why it is written
/// down rather than quietly contradicted. That rule separates *waiting on a
/// person* from *waiting on a system*, and it still holds — on the
/// [`RunStatus`], where [`WaitingApproval`](RunStatus::WaitingApproval) remains
/// distinct from [`Paused`](RunStatus::Paused) and the console renders it
/// "Waiting on you". What the rule does not answer is *has the work happened
/// yet*, and that is the question the **column** asks. The two questions get two
/// answers in two places instead of one column trying to carry both — the same
/// split [`crate::harness::lifecycle::run_status_for`] already documents, and the
/// same reason `landing_column(Delegated)` diverges from its run status.
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
        // A result somebody still has to accept.
        RunStatus::Succeeded => Some(COLUMN_IN_REVIEW),
        // Stopped short, so there is nothing to accept yet (issue #465). Both
        // park the card; resume is a plain `column → in_progress` PATCH, which
        // re-fires dispatch. `WaitingApproval` keeps saying *who* unblocks it on
        // the run status — the column only says the work has not happened.
        RunStatus::WaitingApproval | RunStatus::Paused => Some(COLUMN_PAUSED),
        // Issue #1861: a blocker is a question for a person, so the work has
        // not happened and the card parks — the same answer the column gives
        // its two neighbours, for the same reason. What is *different* lives on
        // the run status, which says a person owes an answer rather than a
        // decision; the column only ever says whether the work happened.
        //
        // Emphatically not `todo`. Epic #183 §3 sends a card that *cannot*
        // proceed back to To-do, and a blocked card can proceed the moment it
        // is answered — sending it to To-do would file an open question as
        // fresh work and lose the question with it. An unanswered blocker does
        // reach To-do eventually, but through the TTL sweep, carrying its
        // question on the card.
        RunStatus::Blocked => Some(COLUMN_PAUSED),
        // Not reviewable work. Epic #183 §3: a card that cannot proceed returns
        // to To-do carrying the reason, never into a stuck column of its own.
        RunStatus::Failed | RunStatus::Cancelled => Some(COLUMN_TODO),
        // A by-design decline (issue #1809) also returns the card to To-do: the
        // builder declined to automate it, the card body carries the reason, and
        // the deliverable is flipped to `once` so its next dispatch reaches the
        // assignee to do by hand rather than re-entering the builder.
        RunStatus::Declined => Some(COLUMN_TODO),
        // Still in flight. Nothing to land.
        RunStatus::Pending | RunStatus::Running => None,
    }
}

/// The human label for a phase or a stage id (`working` → `Working`,
/// `in_progress` → `In progress`).
///
/// The board's ids are wire words; anything a person reads needs the label.
/// Added for the exported task record (issue #352), which is read by people who
/// have never seen the board and must not be shown `in_review`.
///
/// **Both vocabularies**, because since #1512 the same field can be either: a
/// `TaskCard` carries a phase, a `TaskRecord` carries a stage, and this is
/// called with both. Phases are looked up first — a genuine ambiguity exists
/// (`done` is both) and they agree on it.
///
/// Read out of [`crate::ledger::board`] rather than out of a `match` beside it,
/// so a renamed label is one edit and cannot half-land. The console no longer
/// keeps a copy at all — it reads the same labels off the `tasks` ledger, which
/// is built from the same table.
///
/// An unknown id falls back to itself rather than to a guess, so a stored card
/// carrying a state this build does not know still prints something truthful.
///
/// `pub(crate)`: presentation is not part of the port's contract, and the only
/// consumer is the exported record. Widen it if a second surface needs it.
pub(crate) fn column_label(column: &str) -> &str {
    if let Some(phase) = crate::ledger::board::phase(column) {
        return phase.label;
    }
    crate::ledger::board::column(column).map_or(column, |held| held.label)
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

/// What stands where a withdrawn discussion message's text was (issue #358).
///
/// One constant, two very different readers, and they must not drift:
///
/// * the **read fold** (`server::ops::tasks`) substitutes it so every surface
///   that serves a thread — the console, the task-detail export document —
///   shows the same thing;
/// * the **bundle writer** ([`store::export`](crate::store::export)) substitutes
///   it into `events.jsonl`, so the withdrawn text is not in the bundle at all
///   and an import cannot resurrect it.
///
/// Deliberately a sentence rather than an empty string. A blank row reads as a
/// rendering bug; this says a thing happened, which is the honest report — the
/// row keeps its position, its author and its time, and the console names who
/// withdrew it beside this text.
pub const REDACTED_DISCUSSION_TEXT: &str = "This message was removed.";

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
    ///
    /// Set only when the pass resolved **exactly one** candidate (issue #1106).
    /// Two or more is an open choice, not a proposal, and lands in
    /// [`assignee_candidates`](Self::assignee_candidates) instead — the two are
    /// never both populated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_assignee: Option<String>,
    /// The teammates that plausibly fit, when more than one did (issue #1106).
    ///
    /// Non-empty means the pass **declined to choose** and the card is waiting
    /// on a person: an unassigned card whose planner named two teammates who
    /// could each take it is not a card with a proposal, it is a card with a
    /// question. Empty is every other case — the card was already assigned, or
    /// exactly one candidate resolved (see
    /// [`proposed_assignee`](Self::proposed_assignee)), or none did.
    ///
    /// Additive on the wire like every field above it, so a board holding
    /// pre-#1106 plans reads back unchanged: those carry a `proposedAssignee`
    /// and no candidates, which is exactly the shape a one-candidate pass writes
    /// today.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignee_candidates: Vec<AssigneeCandidate>,
    /// When the pass that produced this brief finished.
    pub planned_at_millis: u64,
}

/// One teammate the planner thinks could take a card, and why (issue #1106).
///
/// The reason is the whole point. A bare list of two ids asks an operator to
/// re-derive the judgement the planner already made; the line beside each is
/// what lets them answer without opening two agent pages.
///
/// `id` is **canonical** — resolved against the roster by the host before it is
/// stored, never the raw string the model emitted. A candidate the roster does
/// not recognise is dropped rather than shown, for the same reason
/// `proposed_assignee` drops one: the console must not offer a pick that the
/// write boundary would then refuse.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssigneeCandidate {
    /// The canonical roster key: a teammate id, or a desk id.
    pub id: String,
    /// One line on why this one fits. Model prose, shown to a person who is
    /// about to decide; nothing dispatches off it.
    pub reason: String,
}

/// One metered planning pass attributed to a task.
///
/// Planning runs outside the attempt machinery, so it has no `run_id`. Keeping
/// each non-zero pass on the card preserves failed and superseded planning
/// spend instead of making the latest successful brief stand in for history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPlanningUsage {
    /// When the model call completed.
    pub at_millis: u64,
    /// Tokens and source-currency USD reported by that call.
    pub usage: crate::ports::types::TokenUsage,
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
/// # A last-resort link is unconditional, and that is the whole design
///
/// An output with **no** artifacts and **no** workflows is not an absence — it
/// is the *trace case*, and it is the common one. Plenty of tasks produce no
/// file (a message sent, a record updated, a question answered), and for those
/// the producer's own record **is** the deliverable. A mandatory last-resort
/// link is what stops "no artifact" degrading into "no link", which is the
/// failure epic #183 §6 exists to close. It is also the fallback when an
/// artifact is later deleted: the pinned artifact link dangles, the producer
/// does not.
///
/// **The invariant is "there is always something to open", not "there is always
/// a `run_id`"** (issue #806). Until that issue this field *was* a bare
/// `run_id: String`, and its own docs argued the mandatory run was the design —
/// conflating the guarantee with the single mechanism that happened to provide
/// it. A run is *an* addressable producer; it is not the only one. An operator
/// chat turn produces things too, and run records stay deliberately reserved for
/// actual work attempts (#183 §4), so such a turn has no run to name and a card
/// it settled could carry no output at all. [`TaskOutputSource`] is that
/// distinction made explicit: the guarantee is unchanged and now type-level,
/// while what satisfies it is a closed set the console matches on.
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
    /// What produced it — and the link of last resort (see the type's docs).
    ///
    /// Flattened, so a [`Run`](TaskOutputSource::Run) source is *byte-identical*
    /// on the wire to the `runId` + `attempt` pair this struct carried before
    /// [`TaskOutputSource`] existed. That is what makes this need no migration:
    /// every stored card deserializes into the variant it always meant.
    #[serde(flatten)]
    pub source: TaskOutputSource,
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

/// What produced a [`TaskOutput`] — and, when nothing else survives, what the
/// card's link of last resort points at (issue #806).
///
/// # Why this is a sum and not an optional `run_id`
///
/// Making `run_id` optional would have been honest about the absence and silent
/// about what is actually there, leaving every reader to re-derive "then what do
/// I link to?" and the console's trace arm to degrade to the card even for a
/// turn that demonstrably produced a workflow. Synthesising a run row for a chat
/// turn was the other candidate and is worse: it would reopen #183 §4 on purpose
/// — *"run records stay reserved for actual work attempts, so the Attempts list
/// shows turns that did something"* — and put an id that addresses nothing real
/// behind a type that promises it does.
///
/// A closed set keeps the guarantee the type has always made (there is always
/// one more link) while saying which kind of thing is at the other end, so the
/// console can label it correctly instead of calling a conversation an attempt.
///
/// # Wire compatibility
///
/// `#[serde(untagged)]` plus the `#[serde(flatten)]` on
/// [`TaskOutput::source`] means [`Run`](Self::Run) reads and writes exactly the
/// `runId` / `attempt` keys the struct used before this type existed. Stored
/// cards need no migration and no dual-read: they deserialize into `Run`, which
/// is what they always were. A `ChatTurn` is new keys on a new record only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
// `rename_all` on an enum renames the VARIANTS, not their fields, and an
// untagged enum never writes a variant name — so it would have silently done
// nothing here while the fields went out as `run_id`/`chat_id` and broke every
// stored card. `rename_all_fields` is the attribute that reaches them.
#[serde(untagged, rename_all_fields = "camelCase")]
pub enum TaskOutputSource {
    /// A work attempt — the original and still overwhelmingly common case.
    Run {
        /// The attempt that produced it.
        run_id: String,
        /// That attempt's 1-based ordinal, for the operator-facing label
        /// (*"attempt 2"*).
        ///
        /// `None` when the run row could not be read at stamp time. The ordinal
        /// is a label, never an identity: `run_id` addresses the attempt either
        /// way, so a failed read costs a nicety and never a link.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
    },
    /// An operator chat turn that produced something without a work attempt
    /// behind it — a turn that authored a workflow inline with `create_workflow`
    /// being the case that forced this (PR #805, issue #678).
    ///
    /// Carries the conversation, which is the durable, addressable record of
    /// what the operator asked for and what came back. It is deliberately **not**
    /// a run: minting one would make the Attempts list claim work was attempted.
    ChatTurn {
        /// The conversation the turn belongs to — [`TaskRecord::origin_chat_id`]
        /// for the card this settles.
        chat_id: String,
    },
}

impl TaskOutputSource {
    /// The run that produced this, when one did.
    ///
    /// The accessor exists so the many readers that only ever wanted "which run,
    /// if any" do not each grow a `match`. A chat turn answers `None`, which is
    /// the truthful answer to a question about runs — never an absence of a
    /// *link*, which [`TaskOutput`] still always has.
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::Run { run_id, .. } => Some(run_id.as_str()),
            Self::ChatTurn { .. } => None,
        }
    }

    /// That attempt's 1-based ordinal, when there is an attempt and it was
    /// readable.
    pub fn attempt(&self) -> Option<u32> {
        match self {
            Self::Run { attempt, .. } => *attempt,
            Self::ChatTurn { .. } => None,
        }
    }
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

// ---------------------------------------------------------------------------
// The plan → workflow bridge (issue #580)
// ---------------------------------------------------------------------------

/// Whether a card's In-Progress work produces a one-off result or a reusable
/// workflow (issue #580).
///
/// # The operator chooses; nothing guesses
///
/// This is an **explicit** choice (decision D2a): there is no heuristic that
/// reads a card and decides it "looks automatable". [`Once`](Self::Once) is the
/// default and the historical behaviour — entering In Progress dispatches the
/// card to its assignee exactly as every card did before #580.
/// [`Workflow`](Self::Workflow) routes the card through the **builder pass**
/// (`crate::harness::workflow_build`) instead, which proposes a graph that lands
/// In Review for approval *before* the workflow exists (decision D2b).
///
/// Additive on the wire: [`serde(default)`] makes a card written before this
/// field load as [`Once`](Self::Once), and the field is skipped on serialize
/// when it is `once` (see [`is_once`](Self::is_once)), so a one-off card stays
/// byte-identical to a pre-#580 card on all three backends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskDeliverable {
    /// Do the work once. The card dispatches to its assignee — the behaviour
    /// every card had before #580.
    #[default]
    Once,
    /// Build a reusable workflow. The card routes through the builder pass,
    /// whose proposal lands In Review for approval before the graph is created.
    Workflow,
}

impl TaskDeliverable {
    /// The wire word, for a note line a person reads or a log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Workflow => "workflow",
        }
    }

    /// Whether this is the default one-off deliverable — the `skip_serializing_if`
    /// predicate that keeps a card written before #580 byte-identical on the wire.
    pub fn is_once(&self) -> bool {
        matches!(self, Self::Once)
    }
}

/// A workflow the builder pass proposed for a `workflow`-deliverable card,
/// awaiting operator approval (issue #580).
///
/// # A proposal is not a saved workflow (review before creation)
///
/// The builder pass generates a graph, courtesy-validates that it *could* be
/// created (shape, byte cap, roster) and stamps it here; the card then lands In
/// Review. The graph **does not exist** in the workflow list yet — there is no
/// disabled ghost draft (decision D2b). Only an explicit apply
/// (`POST …/tasks/{id}/workflow-proposal/apply`) runs
/// [`create_company_workflow`](crate::company::create_company_workflow), the one
/// authoring path, under the company write lock — where #276's create-disarm
/// independently lands any schedule-carrying graph switched off until a person
/// arms it.
///
/// # `ops` is the stored authoring payload, and the host is its authority
///
/// [`ops`](Self::ops) holds the `{id, name, description, nodes, edges}` graph the
/// apply route rebuilds a `RawWorkflow` from — the same shape `POST …/workflows`
/// accepts. Storing the payload rather than a rendered graph keeps the host in
/// charge: apply re-derives and re-validates from this, so a proposal cannot
/// smuggle a graph past `create_company_workflow`'s checks. The builder bounds it
/// to the same node/edge/byte caps `create_company_workflow` enforces, so a
/// proposal that could never apply never reaches In Review.
///
/// Additive on the wire ([`serde(default)`] + skip-if-none), like
/// [`TaskRecord::plan`] and [`TaskRecord::output`], so no stored board needs
/// migrating on any backend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskWorkflowProposal {
    /// One line on what the proposed workflow does — what the review panel leads
    /// with.
    pub summary: String,
    /// The `{id, name, description, nodes, edges}` authoring payload the apply
    /// route rebuilds and re-validates the workflow from. Opaque here on the
    /// port; the company layer (`crate::company`) owns its shape.
    pub ops: serde_json::Value,
    /// Epoch-millis the builder pass produced this proposal.
    pub generated_at_millis: u64,
    /// The attempt that built it (issues #339 / #242) — the run whose spend this
    /// proposal accounts for, and the link stamped onto [`TaskOutput::run_id`]
    /// when the proposal is applied. Mandatory: a proposal is always the product
    /// of a metered build attempt, so it always has an attempt to point at.
    pub run_id: String,
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
    /// Every non-zero planning pass, including failed and superseded passes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub planning_attempts: Vec<TaskPlanningUsage>,
    /// Whether this card produces a one-off result or a reusable workflow
    /// (issue #580).
    ///
    /// Defaults to [`TaskDeliverable::Once`] — the behaviour every card had
    /// before #580, and what a card written before this field loads as. Skipped
    /// on the wire when `once`, so a one-off card stays byte-identical to a
    /// pre-#580 card. The operator sets it explicitly; nothing infers it.
    #[serde(default, skip_serializing_if = "TaskDeliverable::is_once")]
    pub deliverable: TaskDeliverable,
    /// The workflow the builder pass proposed for this card, awaiting approval
    /// (issue #580).
    ///
    /// `None` until a `workflow`-deliverable card's builder pass succeeds and
    /// stamps a proposal (the card is then In Review); cleared on reject and once
    /// applied. Additive on the wire like [`Self::plan`] and [`Self::output`], so
    /// no stored board needs migrating. See [`TaskWorkflowProposal`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_proposal: Option<TaskWorkflowProposal>,
    /// The workflow **run** whose agent node opened this card (issue #661 / M5).
    ///
    /// A workflow node's turn may reach `spawn_task` like any other turn, but a
    /// run has no card and no conversation behind it — so the two lineage fields
    /// above have nothing to say about where the card came from, and before this
    /// nothing did. This is that provenance: a **reference**, not a parent.
    ///
    /// # Why a reference rather than a parent
    ///
    /// The alternative was minting a card for the run itself and parenting
    /// spawned cards to it. A daily schedule would then leave 365 operator-facing
    /// run-cards a year, most of them empty ceremony, and it would duplicate a
    /// surface the run already has first-class — the runs history panel and the
    /// `WorkflowRunStarted`/`WorkflowRunFinished` journal pair. So the card is a
    /// lineage **root**: [`parent_task_id`](Self::parent_task_id) and
    /// [`origin_chat_id`](Self::origin_chat_id) are both `None` by construction on
    /// this path, and machine provenance rides these two fields instead.
    ///
    /// # A sub-workflow child stamps its **parent** run's id
    ///
    /// `StoreWorkflowResolver` runs a `sub_workflow` child inside the engine under
    /// the parent's capability bundle — same runner, same run id, and the child
    /// journals no started/finished pair of its own (which is why issue #617
    /// exists). The parent's is therefore the only run identity that exists on
    /// that path, and the only run row a console can navigate to. Pinned by test
    /// rather than left to be rediscovered.
    ///
    /// # A card opened from chat stamps its **turn's** id (issue #983)
    ///
    /// A chat turn now mints a run row of its own, so the field's meaning is
    /// "the machine act that opened this card" rather than "the workflow run
    /// that did". A card raised from a DM used to be the only visible sign that
    /// a long turn was under way and had nothing pointing back at it, so an
    /// operator staring at a card in Planning could not reach the attempt
    /// working it. [`origin_workflow_id`](Self::origin_workflow_id) stays `None`
    /// on that path — there is no graph behind a chat turn — so the two fields
    /// are no longer set together, and a reader wanting *the workflow* must
    /// check that one rather than infer it from this.
    ///
    /// `None` for a dispatched card, a card opened straight on the board, and
    /// for every card written before this field existed — additive on the wire
    /// like [`Self::parent_task_id`], so no stored board needs migrating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_run_id: Option<String>,
    /// The workflow graph whose run opened this card (issue #661 / M5).
    ///
    /// Carried beside [`origin_run_id`](Self::origin_run_id) rather than derived
    /// from it, for the same reason `WorkflowRunFinished` carries both: a run id
    /// is only resolvable to a workflow by folding the journal, and the journal is
    /// trimmable while the board is not. A card outliving its run's rows must
    /// still be able to say *which workflow* opened it.
    ///
    /// Always `Some` when [`origin_run_id`](Self::origin_run_id) is, and `None`
    /// whenever it is — the two are stamped together by the one call site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_workflow_id: Option<String>,
    /// Why a failed or cancelled run returned this card to [`COLUMN_TODO`]
    /// (issue #1865) — set the instant the settle writes that landing, cleared
    /// the instant the card leaves `todo` any other way.
    ///
    /// **The gap this closes**: `todo` was both the failure state and the
    /// fresh state, and a bounced card looked identical to one nobody had
    /// touched yet — an operator had to open every card in the column to tell
    /// them apart. This is the chip the board renders instead, so the
    /// distinction is visible in the grid.
    ///
    /// `None` is the honest default for everything this is not: a card that
    /// has never bounced, one that bounced and was then re-dispatched (cleared
    /// on the `todo` → `in_progress` transition — a fresh attempt earns a
    /// fresh reading, not a stale chip from the last one), one dragged to
    /// `todo` by an operator rather than a run, and every card written before
    /// this field existed. Additive on the wire like [`Self::plan`] and
    /// [`Self::output`], so no stored board needs migrating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounced: Option<String>,
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
        // Issue #465: a run parked on an approval stopped short of a result, so
        // it parks the card rather than presenting it as reviewable work.
        assert_eq!(
            column_for_settled_run(RunStatus::WaitingApproval),
            Some(COLUMN_PAUSED)
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
        // Issue #1809: a by-design decline returns the card to To-do, same as a
        // failure or cancel — the reason is on the note and the card becomes a
        // one-off, never a stuck column of its own.
        assert_eq!(
            column_for_settled_run(RunStatus::Declined),
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
            RunStatus::Declined,
        ] {
            assert_ne!(
                column_for_settled_run(status),
                Some(COLUMN_DONE),
                "{status} must not auto-advance a card to Done"
            );
        }
    }

    /// **Issue #465, stated as the rule rather than the value.** Only a run that
    /// produced a result may land in the column a review verdict consumes.
    ///
    /// The teeth: `review_landing_column(Approve)` turns [`COLUMN_IN_REVIEW`]
    /// into [`COLUMN_DONE`] in one gesture, and it is the only route to Done. A
    /// run that stopped at an unauthorised call has produced nothing to accept —
    /// in the reported case, nothing at all — so leaving it reviewable put
    /// unstarted work one click from finished. #337 removed the automatic route
    /// to that state; this removes the manual one.
    ///
    /// Written as a loop over "did this run produce a result" rather than as an
    /// equality on `WaitingApproval`, so a future status that also stops short
    /// has to answer the same question instead of inheriting a column.
    #[test]
    fn only_a_run_that_produced_a_result_lands_where_review_can_approve_it() {
        for status in [
            RunStatus::WaitingApproval,
            RunStatus::Paused,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::Declined,
        ] {
            assert_ne!(
                column_for_settled_run(status),
                Some(COLUMN_IN_REVIEW),
                "{status} produced nothing to review, so it must not present as \
                 reviewable work a verdict could approve straight to Done"
            );
        }
        // The converse, so this cannot be satisfied by emptying the column: a
        // run that *did* produce a result still reaches the reviewer.
        assert_eq!(
            column_for_settled_run(RunStatus::Succeeded),
            Some(COLUMN_IN_REVIEW)
        );
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
            RunStatus::Declined,
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
            assignee_candidates: Vec::new(),
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
            planning_attempts: Vec::new(),
            deliverable: TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        }
    }

    /// Issue #661 (M5): the run reference round-trips as camelCase, and — the
    /// part that matters — a card written before it existed still loads.
    ///
    /// The legacy half is the load-bearing one. All three backends persist a card
    /// as an opaque JSON blob, so `#[serde(default)]` is the entire migration:
    /// a board written yesterday must deserialize today with both ids `None`,
    /// not fail to load. And a card that carries no run reference must serialize
    /// **without the keys at all**, so every existing card's stored bytes are
    /// unchanged rather than merely equivalent.
    #[test]
    fn a_run_reference_round_trips_and_is_absent_on_a_legacy_card() {
        let mut card = plain_card();
        card.origin_run_id = Some("run-9".to_string());
        card.origin_workflow_id = Some("digest".to_string());

        let json = serde_json::to_string(&card).expect("serialize");
        assert!(json.contains("\"originRunId\":\"run-9\""), "{json}");
        assert!(json.contains("\"originWorkflowId\":\"digest\""), "{json}");
        let back: TaskRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, card);

        // A card with no run reference keeps the pre-#661 wire shape exactly.
        let plain = plain_card();
        let json = serde_json::to_string(&plain).expect("serialize");
        assert!(!json.contains("originRunId"), "{json}");
        assert!(!json.contains("originWorkflowId"), "{json}");

        // And a payload written before the fields existed still loads.
        let legacy = serde_json::json!({
            "id": "t-legacy",
            "title": "Older card",
            "column": "todo",
            "priority": "medium",
            "assignee": "",
            "updatedAtMillis": 3
        });
        let loaded: TaskRecord = serde_json::from_value(legacy).expect("a pre-#661 card loads");
        assert_eq!(loaded.origin_run_id, None);
        assert_eq!(loaded.origin_workflow_id, None);
    }

    /// Issue #339: the whole stamp round-trips as camelCase, and — the part
    /// that matters — a card written before it existed still loads, with
    /// `None`. All three backends persist a card as an opaque JSON blob, so a
    /// `#[serde(default)]` field is the entire migration.
    #[test]
    fn an_output_stamp_round_trips_and_is_absent_on_a_legacy_card() {
        let mut card = plain_card();
        card.output = Some(TaskOutput {
            source: TaskOutputSource::Run {
                run_id: "run-2".to_string(),
                attempt: Some(2),
            },
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
            source: TaskOutputSource::Run {
                run_id: "run-1".to_string(),
                attempt: Some(1),
            },
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
        assert_eq!(output.source.run_id(), Some("run-1"));
        assert!(output.artifacts.is_empty());
        assert!(output.workflows.is_empty());
    }

    /// The attempt ordinal is a label, not an identity: a stamp written when
    /// the run row could not be read still addresses its attempt.
    #[test]
    fn an_unknown_attempt_ordinal_still_leaves_an_addressable_link() {
        let output = TaskOutput {
            source: TaskOutputSource::Run {
                run_id: "run-9".to_string(),
                attempt: None,
            },
            at_millis: 5,
            artifacts: Vec::new(),
            workflows: Vec::new(),
        };
        let json = serde_json::to_string(&output).expect("serialize");
        assert!(!json.contains("attempt"), "{json}");
        let back: TaskOutput = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back.source.run_id(), Some("run-9"));
        assert_eq!(back.source.attempt(), None);
    }

    // --- issue #806: an output whose producer is not a run --------------------

    /// **The migration guarantee.** Every card written before `TaskOutputSource`
    /// existed carries a bare `runId` + `attempt` pair, and there is no dual-read
    /// anywhere — so if this stops deserializing into `Run`, every stamped card
    /// in every store silently loses its link.
    ///
    /// The literal here is deliberately hand-written rather than produced by
    /// serializing the current type: a test that round-trips today's shape would
    /// still pass if both halves drifted together, which is exactly the failure
    /// it is supposed to catch.
    #[test]
    fn a_stamp_written_before_the_source_union_still_reads_as_a_run() {
        let stored = r#"{"runId":"run-7","attempt":2,"atMillis":1234}"#;
        let output: TaskOutput =
            serde_json::from_str(stored).expect("a pre-#806 stamp must still load");
        assert_eq!(output.source.run_id(), Some("run-7"));
        assert_eq!(output.source.attempt(), Some(2));
        assert_eq!(output.at_millis, 1234);
    }

    /// And the other direction: a `Run` source must still *write* those exact
    /// keys, so a card stamped by this build is readable by anything that has
    /// not been updated — and by the console's `"runId" in output` discriminator.
    #[test]
    fn a_run_source_serializes_to_the_keys_it_always_did() {
        let output = TaskOutput {
            source: TaskOutputSource::Run {
                run_id: "run-7".to_string(),
                attempt: Some(2),
            },
            at_millis: 1234,
            artifacts: Vec::new(),
            workflows: Vec::new(),
        };
        let json = serde_json::to_string(&output).expect("serialize");
        assert!(json.contains(r#""runId":"run-7""#), "{json}");
        assert!(json.contains(r#""attempt":2"#), "{json}");
        assert!(
            !json.contains("chatId") && !json.contains("source"),
            "the union must be flattened, not nested or tagged: {json}"
        );
    }

    /// A chat turn's stamp carries the conversation and **no** run — the whole
    /// point of #806. `run_id()` answering `None` is the truthful answer to a
    /// question about runs, and is what stops a reader labelling a conversation
    /// as an attempt.
    #[test]
    fn a_chat_turn_stamp_carries_a_conversation_and_no_run() {
        let output = TaskOutput {
            source: TaskOutputSource::ChatTurn {
                chat_id: "chat-3".to_string(),
            },
            at_millis: 9,
            artifacts: Vec::new(),
            workflows: vec![TaskOutputWorkflow {
                workflow_id: "wf-1".to_string(),
                run_id: None,
                action: TaskOutputAction::Created,
            }],
        };
        let json = serde_json::to_string(&output).expect("serialize");
        assert!(json.contains(r#""chatId":"chat-3""#), "{json}");
        assert!(
            !json.contains("runId\":\"chat"),
            "a chat turn must never be written as a run: {json}"
        );

        let back: TaskOutput = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back.source.run_id(), None);
        assert_eq!(back.source.attempt(), None);
        assert_eq!(
            back.source,
            TaskOutputSource::ChatTurn {
                chat_id: "chat-3".to_string()
            }
        );
        assert_eq!(
            back.workflows.len(),
            1,
            "the deliverable it produced is what makes the link worth having"
        );
    }

    // --- The plan → workflow bridge (issue #580) -----------------------------

    /// The deliverable enum's wire words are pinned — `once`/`workflow` are the
    /// values the REST boundary validates and the operator's toggle sends. A
    /// rename here is a deliberate edit, not an accident.
    #[test]
    fn the_deliverable_wire_words_are_stable() {
        assert_eq!(TaskDeliverable::default(), TaskDeliverable::Once);
        assert_eq!(TaskDeliverable::Once.as_str(), "once");
        assert_eq!(TaskDeliverable::Workflow.as_str(), "workflow");
        assert!(TaskDeliverable::Once.is_once());
        assert!(!TaskDeliverable::Workflow.is_once());
        // Serde uses the same lowercase words the operator's payload carries.
        assert_eq!(
            serde_json::to_string(&TaskDeliverable::Workflow).unwrap(),
            "\"workflow\""
        );
        let back: TaskDeliverable = serde_json::from_str("\"once\"").unwrap();
        assert_eq!(back, TaskDeliverable::Once);
    }

    /// The whole additive-wire contract for #580: a card written before it loads
    /// as a one-off with no proposal, a one-off card stays byte-identical to a
    /// pre-#580 card (no `deliverable` key), and a workflow card with a proposal
    /// round-trips intact. This is what makes "no migration on any backend" true.
    #[test]
    fn the_deliverable_and_proposal_fields_are_additive_on_the_wire() {
        // A pre-#580 blob: no `deliverable`, no `workflowProposal`.
        let legacy = r#"{
            "id": "t-1",
            "title": "Unbridged work",
            "column": "todo",
            "priority": "medium",
            "assignee": "maya",
            "updatedAtMillis": 7
        }"#;
        let card: TaskRecord = serde_json::from_str(legacy).expect("a pre-#580 card parses");
        assert_eq!(card.deliverable, TaskDeliverable::Once);
        assert!(card.workflow_proposal.is_none());

        // A once card grows neither key — byte-identical to the pre-#580 shape.
        let round_tripped = serde_json::to_string(&card).unwrap();
        assert!(
            !round_tripped.contains("deliverable"),
            "a once card must not grow a deliverable key: {round_tripped}"
        );
        assert!(
            !round_tripped.contains("workflowProposal"),
            "an unproposed card must not grow a proposal key: {round_tripped}"
        );

        // A workflow card carrying a proposal round-trips whole.
        let proposed = TaskRecord {
            planning_attempts: Vec::new(),
            deliverable: TaskDeliverable::Workflow,
            workflow_proposal: Some(TaskWorkflowProposal {
                summary: "Email the weekly digest every Monday".to_string(),
                ops: serde_json::json!({
                    "id": "weekly-digest",
                    "name": "Weekly digest",
                    "nodes": [{ "id": "t", "kind": "trigger" }],
                    "edges": []
                }),
                generated_at_millis: 99,
                run_id: "run-7".to_string(),
            }),
            column: COLUMN_IN_REVIEW.to_string(),
            ..card
        };
        let json = serde_json::to_string(&proposed).unwrap();
        assert!(json.contains("\"deliverable\":\"workflow\""), "{json}");
        assert!(json.contains("\"workflowProposal\":"), "{json}");
        assert!(json.contains("\"runId\":\"run-7\""), "{json}");
        let back: TaskRecord = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back, proposed);
    }
}
