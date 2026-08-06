//! The runtime journal: durable at-most-once effect execution and the
//! persistent approval queue.
//!
//! The journal is distinct from the [`EventLog`](crate::ports::EventLog).
//! [`CompanyEvent`](crate::ports::CompanyEvent) is a closed, binding enum with
//! no marker variants, so effect-execution and approval-parking markers cannot
//! ride the event log. They live here instead, in a per-company `journal.jsonl`
//! that boot replay reads back to rebuild in-flight state.
//!
//! Two guarantees:
//!
//! * **At-most-once effects.** Before a side effect runs, its idempotency key is
//!   committed to the journal. On recovery the committed key is skipped, so a
//!   crash after the commit but before the side effect drops the effect (at
//!   most once) rather than repeating it.
//! * **Durable approvals.** Parked effects are journaled and rehydrated on boot,
//!   so an approval survives a restart with its original [`ApprovalId`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::types::{Actor, ApprovalId, Effect};
use crate::runtime::grants::{GrantId, GrantedCall, StandingGrant};
pub use crate::runtime::types::TaskLink;
use crate::store::fs::{PathLocks, append_line};

/// Journal append locks, keyed by path, shared by every [`RuntimeJournal`] in
/// the process (issue #386).
///
/// The lock this replaced was a field on `RuntimeJournal`, so two journals over
/// one file serialised against nothing — which is the state the type has always
/// been in, and which nothing stopped a caller reaching. A `static` is the only
/// thing two independently-constructed instances can share.
///
/// In-process only, and deliberately so: a second *process* on the same
/// `OPENCOMPANY_DATA_DIR` is outside any lock's reach. What keeps that case from
/// tearing is [`append_line`]'s single `O_APPEND` write, not this.
static JOURNAL_WRITE_LOCKS: LazyLock<PathLocks> = LazyLock::new(PathLocks::default);

/// One durable journal record.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "record")]
enum JournalRecord {
    /// A side effect committed to run under this idempotency key.
    EffectExecuted {
        /// The effect's idempotency key.
        key: String,
        /// What the key committed (issue #351).
        ///
        /// The key alone answers "has this run?" and nothing else, which is all
        /// the at-most-once guarantee needs and not nearly enough to tell an
        /// operator what a previous attempt already did. Absent on records
        /// written before #351 — those replay as an executed key with no
        /// description, exactly as they behaved before, and set
        /// [`State::undescribed_executed`] so the console can say so instead of
        /// implying the gap is an all-clear.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<ExecutedEffect>,
    },
    /// An effect parked for operator approval.
    ApprovalParked {
        /// The parked approval's id.
        id: ApprovalId,
        /// The parked effect.
        effect: Effect,
        /// Epoch-millis the effect was parked.
        at_millis: u64,
        /// Which board task this effect was parked for (issue #333).
        ///
        /// This is the correlation key that makes a task's Approvals tab
        /// possible. Before it, an approval carried nothing tying it to a card,
        /// so the only join available was "did this resolve while that task was
        /// running" — a time window, which a second task worked in the same
        /// window silently absorbs.
        ///
        /// **Always written from #333 onward**, as either
        /// [`TaskLink::Task`] or [`TaskLink::Unlinked`] — never omitted. That is
        /// the whole point of the enum over a bare `Option<String>`: "parked for
        /// no card" and "parked by a host that did not record cards" are
        /// different facts, and only the second may fall back to the run window.
        /// Collapsing them sent every workflow delivery, chat turn and scheduler
        /// tick to whatever card happened to be running.
        ///
        /// `None` therefore means exactly one thing: a journal line written
        /// before this field existed. `#[serde(default)]` is what lets those
        /// replay instead of failing to parse.
        #[serde(default)]
        task: Option<TaskLink>,
        /// Which **chat thread** produced the parking cycle (issue #379) — the
        /// desk id for a channel, the roster agent id for a direct message.
        ///
        /// The correlation key that lets an approval be raised in the
        /// conversation that asked for it. [`Effect::agent`] cannot do that job:
        /// a desk channel and a direct message to that desk's lead resolve to
        /// the same agent id, so placing the card by asker would raise a
        /// channel's request inside the lead's private DM.
        ///
        /// A plain `Option<String>` rather than a [`TaskLink`]-style enum,
        /// because nothing downstream falls back to a heuristic when it is
        /// absent: an approval with no thread matches no channel filter and
        /// stays Approvals-page-only, which is exactly today's behaviour. So
        /// "parked by a host that did not record threads" and "parked by a turn
        /// with no conversation behind it" need not be told apart — both mean
        /// "no channel owns this", and both are correct.
        ///
        /// `#[serde(default)]` is what lets a pre-#379 line replay.
        #[serde(default)]
        thread: Option<String>,
        /// Which **cycle** parked it (issue #469) — the turn key.
        ///
        /// The three keys above all answer "what is this approval about". This
        /// one answers "what is waiting on it", and only it can: a single turn
        /// can park several calls, and each of the others is either shared by
        /// turns that are not blocked on each other (a thread hosts many turns)
        /// or absent for the case that matters most (a chat turn has no card and
        /// no run).
        ///
        /// Without it, resolving four sign-offs from one turn re-ran that turn
        /// four times, because nothing could say the four belonged together.
        /// With it, the runtime holds the continuation until the last of a
        /// turn's approvals is decided and then runs it once — see
        /// [`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue).
        ///
        /// `None` means a line written before this field existed, and falls back
        /// to the pre-#469 behaviour of continuing that approval on its own.
        /// `#[serde(default)]` is what lets those lines replay.
        #[serde(default)]
        cycle: Option<String>,
    },
    /// A parked approval that has since been resolved (approved or denied).
    ApprovalResolved {
        /// The resolved approval's id.
        id: ApprovalId,
    },
    /// A parked approval that expired to a default-deny with no operator action.
    ApprovalExpired {
        /// The expired approval's id.
        id: ApprovalId,
        /// Epoch-millis the expiry was recorded.
        at_millis: u64,
    },
    /// A parked approval the operator approved with an amended effect payload.
    ///
    /// Audit-only: the queue removal is recorded by the paired
    /// [`ApprovalResolved`](JournalRecord::ApprovalResolved). The original
    /// effect stays recoverable from the earlier
    /// [`ApprovalParked`](JournalRecord::ApprovalParked), so the immutable log
    /// shows both what was requested and what the operator approved.
    ApprovalAmended {
        /// The amended approval's id.
        id: ApprovalId,
        /// The operator-amended effect that was executed.
        amended_effect: Effect,
        /// Epoch-millis the amendment was recorded.
        at_millis: u64,
    },
    /// A single-use grant minted because the operator approved a tool call an
    /// agent had been blocked from making (issue #243).
    ///
    /// This is the durable audit line for "the operator said yes to *this*
    /// call": it carries the agent, the tool, and the exact arguments admitted,
    /// which is more than the event log's
    /// [`ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved) can
    /// hold. Written *before* the grant reaches the live set, so a crash between
    /// the two re-arms it on replay rather than losing the operator's decision.
    ApprovalGranted {
        /// The grant, whole.
        grant: GrantedCall,
    },
    /// A grant redeemed by its agent — the tool ran.
    GrantConsumed {
        /// The consumed grant's approval id.
        id: ApprovalId,
        /// What the redeemed grant actually did (issue #351).
        ///
        /// An approved *agent tool call* never reaches
        /// [`EffectExecuted`](Self::EffectExecuted): it is settled by minting a
        /// grant, and the tool then runs inside the agent's next turn. This
        /// record is therefore the only line in the journal that means "an
        /// operator-approved `composio_execute` payment fired", and without a
        /// description on it the retry dialog would open naming the native
        /// email beside it and nothing else — a confirmation understating what
        /// already happened.
        ///
        /// Absent on records written before this field existed; those replay as
        /// a consumed grant with no description, the same additive contract
        /// [`EffectExecuted`](Self::EffectExecuted) has.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<ExecutedEffect>,
    },
    /// A grant that expired unredeemed past [`GRANT_TTL_MILLIS`](crate::runtime::grants::GRANT_TTL_MILLIS).
    GrantExpired {
        /// The expired grant's approval id.
        id: ApprovalId,
        /// Epoch-millis the expiry was recorded.
        at_millis: u64,
    },
    /// A **standing** grant minted because the operator chose the broader scope
    /// on an approval: this tool, for this teammate, until a deadline (#374).
    ///
    /// Carries the grant whole, like
    /// [`ApprovalGranted`](Self::ApprovalGranted), because this line is the only
    /// durable answer to "who opened this tool up, when, off which card, and
    /// until when". `StandingGrant::granted_by` is the operator's real identity,
    /// not the placeholder the resolve route used to hardcode.
    ///
    /// Written *before* the grant reaches the live set, the same crash direction
    /// `ApprovalGranted` takes.
    StandingGrantMinted {
        /// The standing grant, whole.
        grant: StandingGrant,
    },
    /// A standing grant the operator took back (#374).
    ///
    /// Takes effect on the **next** policy check — an already-admitted call is
    /// not aborted, because there is no abort lever inside an agent's turn and
    /// killing one mid-call is the lifecycle anti-pattern this codebase avoids
    /// elsewhere. The next check finds nothing and re-parks.
    StandingGrantRevoked {
        /// The revoked grant's id.
        id: GrantId,
        /// Who revoked it.
        by: Actor,
        /// Epoch-millis the revocation was recorded.
        at_millis: u64,
    },
    /// A standing grant that reached its deadline (#374).
    StandingGrantExpired {
        /// The expired grant's id.
        id: GrantId,
        /// Epoch-millis the expiry was recorded.
        at_millis: u64,
    },
}

/// A parked approval awaiting resolution.
#[derive(Clone, Debug)]
pub struct PendingApproval {
    /// The approval's id.
    pub id: ApprovalId,
    /// The parked effect.
    pub effect: Effect,
    /// Epoch-millis the effect was parked.
    pub at_millis: u64,
    /// Which board task this approval was parked for (issue #333). `None` only
    /// for a journal line written before the link existed — see [`TaskLink`].
    pub task: Option<TaskLink>,
    /// The chat thread that produced the parking cycle (issue #379) — a desk id
    /// for a channel, a roster agent id for a direct message.
    ///
    /// `None` for a pre-#379 journal line *and* for every park with no
    /// conversation behind it (a workflow delivery, a scheduler tick, a cycle
    /// whose triggers were ambiguous). Both are the same fact downstream: no
    /// channel owns this approval, so it is shown on the Approvals page only.
    pub thread: Option<String>,
}

/// What an approval *was*, retained for the whole life of the journal — after
/// it resolves, expires, or is amended away (issue #333, over #305's index).
///
/// The parked effect itself is dropped from the queue on resolution, and
/// [`CompanyEvent::ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved)
/// carries only an id, a verdict and an actor. So without this index a resolved
/// approval is unreadable: the read side cannot say what was approved, when it
/// parked, or which task it belonged to.
///
/// **Entries are never removed, and the map is unbounded.** It has the same
/// append-only lifetime as the journal file it is replayed from: one resident
/// entry per approval ever parked, for the life of the process, growing
/// without a ceiling. #333 widens each entry from a `u64` to a `u64` plus two
/// `String`s (the effect kind and, when linked, the task id). No rotation
/// exists today, so `load` rebuilding this from every `ApprovalParked` line is
/// the only path — and it is the correct one. If journal rotation ever lands,
/// this index is the first thing that has to survive it, because a rotated-away
/// park line silently turns its approval unreadable.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalOrigin {
    /// Epoch-millis the effect was parked.
    pub at_millis: u64,
    /// The parked effect's dotted kind, e.g. `payment.send`.
    pub kind: String,
    /// Which board task the parking cycle was dispatched for. `None` only for a
    /// pre-#333 journal line — see [`TaskLink`].
    ///
    /// The **card-level** key, and the fallback one: it cannot say which of a
    /// card's attempts parked the approval. See [`run_id`](Self::run_id).
    pub task: Option<TaskLink>,
    /// The attempt this approval was parked under
    /// ([`Effect::run_id`](crate::ports::types::Effect::run_id), issue #242),
    /// copied off the effect at park time so the read side need not re-open it.
    ///
    /// The **attempt-level** key, and the authoritative one where present: a
    /// [`RunRecord`](crate::ports::runs::RunRecord) names its card, so a run id
    /// resolves to a task, while a task id can never resolve to a run. #183
    /// settled that repeat trips through review are normal, so two attempts on
    /// one card is the expected case — and only this key tells them apart.
    ///
    /// `None` by design for every park with no attempt behind it: a chat turn,
    /// a workflow delivery, a scheduler tick, and the hosted brain's own gate.
    /// That is why it cannot be the only key — see [`task`](Self::task).
    pub run_id: Option<String>,
    /// The chat thread the parking cycle answered (issue #379).
    ///
    /// The **conversation-level** key, and orthogonal to the two above: a chat
    /// turn has a thread and no card, a dispatched card has a card and no
    /// thread, and a desk turn triggered from a channel has both. Retained here
    /// (not only on the live queue) so a *resolved* approval's origin thread is
    /// still recoverable — which is what lets a follow-up cycle's own re-park
    /// stay in the channel the first sign-off was asked in.
    pub thread: Option<String>,
    /// The **turn** that parked it: the id of the parking cycle (issue #469).
    ///
    /// The key that groups the several approvals one turn can raise, so the
    /// turn is continued once — after the last of them is decided — instead of
    /// once per decision. `None` for a pre-#469 journal line, which continues
    /// on its own exactly as it used to.
    pub cycle: Option<String>,
}

/// One approval currently waiting in the in-memory queue.
#[derive(Clone, Debug)]
struct ParkedApproval {
    effect: Effect,
    at_millis: u64,
    /// `None` only for a journal line written before #333.
    task: Option<TaskLink>,
    /// The chat thread that parked it (issue #379); `None` when no conversation
    /// produced it, or on a pre-#379 line.
    thread: Option<String>,
    /// The turn that parked it (issue #469); `None` on a pre-#469 line. Held on
    /// the live entry, not only in `origins`, because recovery has to re-arm the
    /// continuation queue from exactly the approvals that are *still* waiting.
    cycle: Option<String>,
}

/// A side effect that was **committed to run** (issue #351): what it was, which
/// board task it was run for, and whether it is one that cannot be taken back.
///
/// "Committed", not "completed", and the distinction is deliberate. The record
/// is written *before* the side effect is performed — that ordering is what
/// makes effects at-most-once — and a failed or interrupted perform leaves it
/// standing. So an entry means "this was committed, and the runtime will never
/// run it again", which is exactly the fact a retry warning needs: the operator
/// has to assume it happened, because nothing else will ever finish it and
/// nothing will re-attempt it. It does **not** mean the effect is known to have
/// completed. Operator-facing wording is qualified to match
/// (`RetryButton`, `frontend/src/views/TaskDetailView.tsx`).
///
/// Recorded alongside the idempotency key so a retry can say what the previous
/// attempt already did. Deliberately **not** the whole [`Effect`]: `payload`
/// carries recipients, message bodies and arguments, and this record is read
/// back out onto an operator's screen through the task-detail route, which
/// scrubs by construction. The classification facts are kept; the contents are
/// not.
///
/// `irreversible` is decided **at execution time**, by the gate that was in
/// force then (`ManifestApprovalGate::is_irreversible`), rather than re-derived
/// on read. A company that later raises its auto-approve cap does not get to
/// retroactively decide that the payment it made last week was routine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutedEffect {
    /// The dotted effect kind, e.g. `payment.send`. The console maps it to
    /// plain language; it is never shown raw.
    pub kind: String,
    /// The USD amount involved, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_usd: Option<f64>,
    /// The board task this effect was executed for, when a card was behind it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Epoch-millis the effect was committed.
    pub at_millis: u64,
    /// Whether the supervised taxonomy calls this one irreversible.
    pub irreversible: bool,
}

/// A journal line [`load`](RuntimeJournal::load) could not replay (issue #386).
///
/// Deliberately carries **no line content**. The journal holds effect payloads —
/// recipients, message bodies, arguments — and a corruption report exists to be
/// logged and read by an operator, which is the one place [`ExecutedEffect`]
/// goes to some trouble to keep those out of. The line number locates it in the
/// file, the byte length separates a merged pair (long) from a truncated tail
/// (short), and the parse error names the column without quoting it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorruptLine {
    /// The line's 1-based number in the journal file.
    pub line: usize,
    /// The line's length in bytes.
    pub bytes: usize,
    /// What the parse rejected.
    pub message: String,
}

/// In-memory state rebuilt from (and kept in sync with) `journal.jsonl`.
#[derive(Default)]
struct State {
    executed: HashSet<String>,
    /// Lines the last replay could not read — see [`CorruptLine`].
    corrupt: Vec<CorruptLine>,
    /// Every irreversible effect that ran for a board task, indexed by that
    /// task and oldest first within it (issue #351).
    ///
    /// Append-only for the same reason [`executed`](Self::executed) is: an
    /// effect that fired stays fired, and a retry warning that forgot half the
    /// history would be worse than none. One small record per effect, with no
    /// payload — see [`ExecutedEffect`].
    ///
    /// Indexed rather than a flat list because the read side is a per-task
    /// lookup on every Task Detail GET, and a linear scan of every effect a
    /// company ever executed is not flat for a long-lived one. Reversible
    /// effects and effects with no card behind them are dropped on the way in:
    /// nothing reads them, and the only thing keeping them would grow is
    /// memory.
    irreversible_by_task: HashMap<String, Vec<ExecutedEffect>>,
    /// Whether replay saw an executed key it cannot describe (issue #351).
    ///
    /// True when a pre-#351 `EffectExecuted` line is read back: the key proves
    /// something ran, and the record carries no way to say what. The retry
    /// dialog's "nothing irreversible here" is only honest when this is false,
    /// so the console is told and confirms regardless — see
    /// [`has_undescribed_history`](RuntimeJournal::has_undescribed_history).
    undescribed_executed: bool,
    parked: HashMap<ApprovalId, ParkedApproval>,
    /// The effect each approval was parked with, **payload scrubbed**, retained
    /// after the approval leaves [`parked`](Self::parked) (issue #351).
    ///
    /// Approving a harness tool call mints a grant rather than executing, so
    /// the only description of what the operator said yes to lives on the park
    /// record. This is what the grant-consumption path reads back to classify
    /// and name it once the tool has actually run. Overwritten by an
    /// approve-with-edit, because the grant is minted against the amended
    /// arguments and the amount the operator approved is the one to report.
    ///
    /// The payload is replaced with `Null` on the way in. Classification reads
    /// only the kind, group, amount and counterparty flags, and this map
    /// outlives the queue entry — retaining recipients and message bodies for
    /// the life of the process to answer a question that never asks for them
    /// would be the one leak [`ExecutedEffect`] exists to avoid.
    approval_effects: HashMap<ApprovalId, Effect>,
    /// What each approval was when it parked, retained after it leaves `parked`.
    ///
    /// This is what makes waiting time readable (issue #305) and what links a
    /// resolved approval back to its board task (issue #333). Both facts are
    /// journal-only — [`CompanyEvent::ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved)
    /// carries the resolution but neither the park time nor the task — so they
    /// are recoverable only by joining the two on [`ApprovalId`]. See
    /// [`ApprovalOrigin`] for why entries are never removed.
    origins: HashMap<ApprovalId, ApprovalOrigin>,
    /// Grants minted and not yet consumed or expired (issue #243).
    ///
    /// Unlike [`origins`](Self::origins) this one IS removed from on
    /// the terminal records: a replayed grant is handed straight back to the
    /// live [`GrantSet`](crate::runtime::grants::GrantSet), so keeping a
    /// consumed or expired entry here would re-arm a tool call that already ran
    /// (or that the operator was already told had lapsed) on every restart.
    grants: HashMap<ApprovalId, GrantedCall>,
    /// Standing grants minted and not yet revoked or expired (issue #374).
    ///
    /// Removed from on both terminal records for the same reason as
    /// [`grants`](Self::grants): a replayed entry is handed straight back to the
    /// live set, so retaining a revoked one would hand back a permission the
    /// operator explicitly took away — on every restart, silently.
    standing_grants: HashMap<GrantId, StandingGrant>,
}

impl State {
    /// Files an executed effect under the card it ran for, keeping only what
    /// the retry warning reads (issue #351).
    ///
    /// Two drops, both deliberate: a reversible effect is never named, and an
    /// effect with no card behind it belongs to no dialog. Retaining either
    /// would grow one map per company for a lookup that filters them straight
    /// back out.
    fn index_executed(&mut self, effect: ExecutedEffect) {
        if !effect.irreversible {
            return;
        }
        let Some(task_id) = effect.task_id.clone() else {
            return;
        };
        self.irreversible_by_task
            .entry(task_id)
            .or_default()
            .push(effect);
    }

    /// Retains an approval's effect for later description, without its payload.
    fn retain_approval_effect(&mut self, id: &ApprovalId, effect: &Effect) {
        self.approval_effects.insert(
            id.clone(),
            Effect {
                payload: serde_json::Value::Null,
                ..effect.clone()
            },
        );
    }
}

/// A per-company append-only journal backing at-most-once effects and the
/// durable approval queue.
///
/// One process should own a given journal file, but [`append`](Self::append) no
/// longer depends on that for integrity (issue #386). Every record is written
/// whole — terminator included — in a single `O_APPEND` write that has reached
/// the kernel before the call returns, so a concurrent writer can land a record
/// before or after but never inside one. Writers in *this* process additionally
/// serialise on [`JOURNAL_WRITE_LOCKS`], which keeps records in call order, so a
/// park cannot be replayed after the resolution that drains it.
pub struct RuntimeJournal {
    path: PathBuf,
    state: StdMutex<State>,
    write_lock: Arc<TokioMutex<()>>,
}

impl RuntimeJournal {
    /// Opens (or prepares) the journal at `path` without loading it.
    ///
    /// Call [`load`](Self::load) to replay an existing journal into memory.
    ///
    /// Two journals over one path share an append lock. The key is the
    /// absolutised path, so a relative and an absolute spelling of one file
    /// match; a symlinked or `..`-laden spelling still does not, and falls back
    /// on the atomic write for its safety.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let write_lock =
            JOURNAL_WRITE_LOCKS.get(&std::path::absolute(&path).unwrap_or_else(|_| path.clone()));
        Self {
            path,
            state: StdMutex::new(State::default()),
            write_lock,
        }
    }

    /// Replays the on-disk journal into memory, reconstructing the executed-key
    /// set and the parked-approval queue. Idempotent.
    ///
    /// **A damaged line does not fail the load** (issue #386). It is skipped,
    /// logged against the file and line number, and reported through
    /// [`corruption`](Self::corruption) for the caller to act on. Before this,
    /// one bad line returned `Err` from here and took the whole company's boot
    /// with it — turning the loss of a single record into the loss of every
    /// record after it, plus the tenant. An operator cannot repair a journal
    /// through a console that will not start.
    ///
    /// The skip is genuinely lossy and the safety argument is not symmetric: a
    /// dropped `ApprovalResolved` leaves an approval parked, which a person can
    /// still deny, while a dropped `EffectExecuted` un-commits a key and lets an
    /// effect run twice. That is why [`replay_line`] recovers a merged line in
    /// full rather than skipping it — the historical corruption this issue is
    /// about is exactly the recoverable kind, and skipping it is the outcome
    /// worth working to avoid.
    pub async fn load(&self) -> Result<()> {
        // Read bytes, not a `String`. A torn write can split a multi-byte
        // codepoint, and `read_to_string` would fail the whole load on that one
        // bad byte — failing the boot for exactly the damage this function
        // exists to survive. Decoding per line instead keeps a single mangled
        // line on the `CorruptLine` path with the rest of the journal intact.
        let contents = match tokio::fs::read(&self.path).await {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(self.io_err(e)),
        };

        let mut state = State::default();
        for (index, raw) in contents.split(|b| *b == b'\n').enumerate() {
            // Lossy on purpose: invalid bytes become U+FFFD, the line then fails
            // to parse as JSON, and it lands on the same skip-and-log path as
            // any other unrecoverable line rather than aborting the replay.
            let line = String::from_utf8_lossy(raw);
            let line = line.as_ref();
            if line.trim().is_empty() {
                continue;
            }
            let records = match replay_line(line) {
                Ok(records) => records,
                Err(message) => {
                    let corrupt = CorruptLine {
                        line: index + 1,
                        bytes: line.len(),
                        message,
                    };
                    tracing::error!(
                        journal = %self.path.display(),
                        line = corrupt.line,
                        bytes = corrupt.bytes,
                        error = %corrupt.message,
                        "journal line could not be replayed; skipping it and continuing",
                    );
                    state.corrupt.push(corrupt);
                    continue;
                }
            };
            if records.len() > 1 {
                // Recovered, not lost — so not a `CorruptLine`. Still worth
                // saying out loud: the file carries damage from a host that
                // predates the write fix, and a reader looking at it by hand
                // should know why one line holds several records.
                tracing::warn!(
                    journal = %self.path.display(),
                    line = index + 1,
                    records = records.len(),
                    "journal line holds several records with no separator; \
                     replaying all of them",
                );
            }
            for record in records {
                Self::replay(&mut state, record);
            }
        }
        *self.state.lock().expect("journal state poisoned") = state;
        Ok(())
    }

    /// Lines the last [`load`](Self::load) could not replay, in file order.
    ///
    /// Empty is the only healthy answer. A non-empty one means the company is
    /// running on an incomplete history and something above has to say so.
    pub fn corruption(&self) -> Vec<CorruptLine> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .corrupt
            .clone()
    }

    /// Folds one replayed record into the rebuilt state.
    fn replay(state: &mut State, record: JournalRecord) {
        match record {
            JournalRecord::EffectExecuted { key, effect } => {
                state.executed.insert(key);
                // Absent on a pre-#351 line: the key still replays, the
                // description simply does not exist to replay. Flag it, so
                // the console says "there is earlier activity I cannot
                // describe" rather than showing an all-clear.
                match effect {
                    Some(effect) => state.index_executed(effect),
                    None => state.undescribed_executed = true,
                }
            }
            JournalRecord::ApprovalParked {
                id,
                effect,
                at_millis,
                task,
                thread,
                cycle,
            } => {
                state.retain_approval_effect(&id, &effect);
                state.origins.insert(
                    id.clone(),
                    ApprovalOrigin {
                        at_millis,
                        kind: effect.kind.clone(),
                        task: task.clone(),
                        run_id: effect.run_id.clone(),
                        thread: thread.clone(),
                        cycle: cycle.clone(),
                    },
                );
                state.parked.insert(
                    id,
                    ParkedApproval {
                        effect,
                        at_millis,
                        task,
                        thread,
                        cycle,
                    },
                );
            }
            JournalRecord::ApprovalResolved { id } => {
                state.parked.remove(&id);
            }
            JournalRecord::ApprovalExpired { id, .. } => {
                state.parked.remove(&id);
            }
            // Audit-only for the queue: the paired `ApprovalResolved`
            // handles removal. The amended effect does supersede the parked
            // one for description, because it is the amended arguments the
            // grant was minted against.
            JournalRecord::ApprovalAmended {
                id, amended_effect, ..
            } => {
                state.retain_approval_effect(&id, &amended_effect);
            }
            JournalRecord::ApprovalGranted { grant } => {
                state.grants.insert(grant.approval_id.clone(), grant);
            }
            JournalRecord::GrantConsumed { id, effect } => {
                state.grants.remove(&id);
                // Absent only on a line written before the grant path was
                // described; same additive contract as `EffectExecuted`.
                if let Some(effect) = effect {
                    state.index_executed(effect);
                }
            }
            JournalRecord::GrantExpired { id, .. } => {
                state.grants.remove(&id);
            }
            JournalRecord::StandingGrantMinted { grant } => {
                state.standing_grants.insert(grant.id.clone(), grant);
            }
            JournalRecord::StandingGrantRevoked { id, .. } => {
                state.standing_grants.remove(&id);
            }
            JournalRecord::StandingGrantExpired { id, .. } => {
                state.standing_grants.remove(&id);
            }
        }
    }

    /// Whether an effect under `key` was already committed.
    pub fn is_executed(&self, key: &str) -> bool {
        self.state
            .lock()
            .expect("journal state poisoned")
            .executed
            .contains(key)
    }

    /// Commits an effect key to the journal before its side effect runs,
    /// alongside a description of what the key is about to do (issue #351).
    ///
    /// A no-op (returns `Ok`) if the key is already committed — which is also
    /// what keeps the executed-effect list free of duplicates: the second
    /// commit under a key never reaches the append.
    pub async fn record_executed(&self, key: &str, effect: ExecutedEffect) -> Result<()> {
        {
            let mut state = self.state.lock().expect("journal state poisoned");
            if !state.executed.insert(key.to_string()) {
                return Ok(());
            }
            state.index_executed(effect.clone());
        }
        self.append(&JournalRecord::EffectExecuted {
            key: key.to_string(),
            effect: Some(effect),
        })
        .await
    }

    /// Records a newly parked approval and which board task it belongs to
    /// (issue #333).
    ///
    /// `task` is deliberately **not** an `Option`: every caller must say which
    /// it is, [`TaskLink::Task`] or [`TaskLink::Unlinked`], so that a missing
    /// link can only ever mean "written before #333". A caller with an
    /// `Option<&str>` in hand converts with [`TaskLink::from_task_id`].
    ///
    /// `thread` **is** an `Option`, and deliberately so (issue #379): unlike the
    /// task link, nothing downstream distinguishes "no conversation produced
    /// this" from "this host does not record conversations". Both mean no
    /// channel owns the approval, and both correctly leave it on the Approvals
    /// page alone.
    ///
    /// `cycle` is the parking turn (issue #469), and is what lets the runtime
    /// continue a turn once rather than once per approval it raised. `Option`
    /// on the same terms as `thread`: absent means "this host did not record a
    /// turn", which falls back to continuing the approval on its own.
    pub async fn record_parked(
        &self,
        id: &ApprovalId,
        effect: &Effect,
        at_millis: u64,
        task: TaskLink,
        thread: Option<String>,
        cycle: Option<String>,
    ) -> Result<()> {
        {
            let mut state = self.state.lock().expect("journal state poisoned");
            state.origins.insert(
                id.clone(),
                ApprovalOrigin {
                    at_millis,
                    kind: effect.kind.clone(),
                    task: Some(task.clone()),
                    run_id: effect.run_id.clone(),
                    thread: thread.clone(),
                    cycle: cycle.clone(),
                },
            );
            state.parked.insert(
                id.clone(),
                ParkedApproval {
                    effect: effect.clone(),
                    at_millis,
                    task: Some(task.clone()),
                    thread: thread.clone(),
                    cycle: cycle.clone(),
                },
            );
            state.retain_approval_effect(id, effect);
        }
        self.append(&JournalRecord::ApprovalParked {
            id: id.clone(),
            effect: effect.clone(),
            at_millis,
            task: Some(task),
            thread,
            cycle,
        })
        .await
    }

    /// The turn key of every approval **still parked**, one entry per approval
    /// (issue #469).
    ///
    /// Read once, at recovery, to re-arm the
    /// [`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue):
    /// a restart in the middle of a partly-decided turn must come back still
    /// knowing that turn is blocked, or its continuation would either fire early
    /// or never fire at all. Approvals with no turn key (pre-#469 lines) are
    /// omitted — they continue on their own and are never gated.
    pub fn parked_turns(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .parked
            .values()
            .filter_map(|p| p.cycle.clone())
            .collect()
    }

    /// The turn that parked `id`, if it is one this journal recorded
    /// (issue #469).
    ///
    /// Two levels of absence, and they mean different things — the same shape
    /// [`approval_thread`](Self::approval_thread) uses. `None`: nothing was ever
    /// parked under this id. `Some(None)`: parked, by a line written before the
    /// turn key existed.
    pub fn approval_cycle(&self, id: &ApprovalId) -> Option<Option<String>> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .get(id)
            .map(|o| o.cycle.clone())
    }

    /// The effect an approval was parked with, payload scrubbed (issue #351).
    ///
    /// Answers the grant-consumption path's question: the agent just redeemed
    /// this approval's grant and the tool ran — what was it, and was it one that
    /// cannot be taken back? Superseded by an approve-with-edit, since that is
    /// what the grant was minted against.
    pub fn approval_effect(&self, id: &ApprovalId) -> Option<Effect> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .approval_effects
            .get(id)
            .cloned()
    }

    /// Whether replay read back an executed key it cannot describe (issue #351).
    ///
    /// Company-wide rather than per-task, and necessarily so: an undescribed
    /// record carries no card either, so there is nothing to attribute it to.
    /// The console's contract is that an empty
    /// [`irreversible_effects`](Self::irreversible_effects) means the journal
    /// holds nothing irreversible for a card — true only when this is `false`.
    /// When it is `true` the console confirms regardless and says the earlier
    /// activity cannot be described, instead of showing an all-clear it cannot
    /// stand behind.
    ///
    /// The related pre-#351 gap it does **not** detect on its own: an approval
    /// parked before the upgrade carries no `task_id`, so approving it
    /// afterwards executes an effect attributed to no card. That record is
    /// byte-identical to a legitimately card-less park written today, so
    /// flagging it would misreport every company that has ever parked an
    /// approval from operator chat. In practice a company old enough to hold a
    /// pre-#351 park also holds pre-#351 executed lines, so this flag is set and
    /// the same warning shows.
    pub fn has_undescribed_history(&self) -> bool {
        self.state
            .lock()
            .expect("journal state poisoned")
            .undescribed_executed
    }

    /// The irreversible effects this task has already executed, oldest first
    /// (issue #351).
    ///
    /// Drawn from the journal's own executed record — the same append-only set
    /// that makes effects at-most-once — rather than re-derived from timeline
    /// labels, which describe what an agent *said* and not what was committed.
    /// A direct index lookup, so a company's history length does not price a
    /// Task Detail read.
    pub fn irreversible_effects(&self, task_id: &str) -> Vec<ExecutedEffect> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .irreversible_by_task
            .get(task_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Records that a parked approval was resolved (removing it from the queue).
    pub async fn record_resolved(&self, id: &ApprovalId) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .parked
            .remove(id);
        self.append(&JournalRecord::ApprovalResolved { id: id.clone() })
            .await
    }

    /// Records that a parked approval expired to a default-deny, removing it
    /// from the queue. This is the durable audit entry for
    /// default-deny-on-silence.
    pub async fn record_expired(&self, id: &ApprovalId, at_millis: u64) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .parked
            .remove(id);
        self.append(&JournalRecord::ApprovalExpired {
            id: id.clone(),
            at_millis,
        })
        .await
    }

    /// Records an operator-amended approval (an approve-with-edit) for the audit
    /// trail. Removal from the queue is recorded separately by
    /// [`record_resolved`](Self::record_resolved).
    pub async fn record_amended(
        &self,
        id: &ApprovalId,
        amended_effect: &Effect,
        at_millis: u64,
    ) -> Result<()> {
        // The amendment supersedes the park as the description of what the
        // operator approved (issue #351) — a grant is minted against the
        // amended arguments, so an edited amount is the one to report.
        self.state
            .lock()
            .expect("journal state poisoned")
            .retain_approval_effect(id, amended_effect);
        self.append(&JournalRecord::ApprovalAmended {
            id: id.clone(),
            amended_effect: amended_effect.clone(),
            at_millis,
        })
        .await
    }

    /// A snapshot of what every approval ever parked *was*, keyed by
    /// [`ApprovalId`] — including approvals since resolved or expired.
    ///
    /// The read side joins this against the event log's
    /// [`ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved) to
    /// recover how long an approval was waiting (issue #305) and which board
    /// task it belonged to (issue #333). Taken as one snapshot per request
    /// rather than per lookup, so a fold never holds the state lock while it
    /// works.
    pub fn approval_origins(&self) -> HashMap<ApprovalId, ApprovalOrigin> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .clone()
    }

    /// What one approval was when it parked, without cloning the whole
    /// [`origins`](State::origins) map.
    ///
    /// The read path resolves a bounded number of ids per request — the
    /// approval events on one page of the fold, plus the parked queue — so it
    /// takes this per id rather than a snapshot. [`approval_origins`] copies an
    /// index that grows with every approval ever parked and is never pruned, and
    /// the task-detail route is polled, so a snapshot there costs the whole
    /// history on every poll.
    ///
    /// [`approval_origins`]: Self::approval_origins
    pub fn approval_origin(&self, id: &ApprovalId) -> Option<ApprovalOrigin> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .get(id)
            .cloned()
    }

    /// The task link recorded for one approval, without cloning the whole
    /// [`origins`](State::origins) map.
    ///
    /// The map is unbounded and never pruned (see [`ApprovalOrigin`]), so a
    /// caller that needs the link for a couple of known ids — every cycle does,
    /// via [`cycle_task_id`](crate::runtime::cycle) — must not pay a full clone
    /// per cycle to read them. `approval_origins` stays the right call for a
    /// fold that will look up an unknown number of ids.
    ///
    /// The outer `Option` is "no such approval"; the inner is a pre-#333 line.
    pub fn approval_task(&self, id: &ApprovalId) -> Option<Option<TaskLink>> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .get(id)
            .map(|o| o.task.clone())
    }

    /// The chat thread recorded for one approval (issue #379), read the same
    /// per-id way as [`approval_task`](Self::approval_task) and for the same
    /// reason — the origins map is unbounded, and a cycle needs at most the
    /// couple of ids in its own batch.
    ///
    /// The outer `Option` is "no such approval"; the inner is "no conversation
    /// behind it" (which a pre-#379 line is indistinguishable from, by design).
    /// Reading it off the retained origin rather than the live queue is what
    /// makes it answerable *after* the approval resolved — the case
    /// [`cycle_thread_id`](crate::runtime::cycle) needs so a second sign-off
    /// re-parks in the channel the first one was asked in.
    pub fn approval_thread(&self, id: &ApprovalId) -> Option<Option<String>> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .get(id)
            .map(|o| o.thread.clone())
    }

    /// Records a minted single-use grant (issue #243).
    ///
    /// Called *before* the grant enters the live set, so the ordering failure
    /// mode is "recorded but not live" — which replay fixes — rather than "live
    /// but not recorded", which a crash would lose silently.
    pub async fn record_granted(&self, grant: &GrantedCall) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .grants
            .insert(grant.approval_id.clone(), grant.clone());
        self.append(&JournalRecord::ApprovalGranted {
            grant: grant.clone(),
        })
        .await
    }

    /// Records that a grant was redeemed — the agent re-issued the call and the
    /// tool ran. Removes it from the replay set so a restart cannot re-arm it.
    ///
    /// `effect` describes what the redeemed call was (issue #351), so an
    /// operator-approved tool call reaches the retry warning at all. This is the
    /// grant path's only chance to be described: it is settled by minting a
    /// grant, not by `execute_effect_once`, so it writes no `EffectExecuted`
    /// line. `None` when the approval's parked effect is no longer recoverable
    /// — the redemption is still recorded, it simply contributes no warning.
    pub async fn record_grant_consumed(
        &self,
        id: &ApprovalId,
        effect: Option<ExecutedEffect>,
    ) -> Result<()> {
        {
            let mut state = self.state.lock().expect("journal state poisoned");
            state.grants.remove(id);
            if let Some(effect) = effect.clone() {
                state.index_executed(effect);
            }
        }
        self.append(&JournalRecord::GrantConsumed {
            id: id.clone(),
            effect,
        })
        .await
    }

    /// Records that a grant expired unredeemed. Same replay removal as
    /// consumption: the operator has been told it lapsed, so a restart must not
    /// quietly hand the agent the permission back.
    pub async fn record_grant_expired(&self, id: &ApprovalId, at_millis: u64) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .grants
            .remove(id);
        self.append(&JournalRecord::GrantExpired {
            id: id.clone(),
            at_millis,
        })
        .await
    }

    /// Every grant still live according to the journal — what boot recovery
    /// seeds the in-memory [`GrantSet`](crate::runtime::grants::GrantSet) with.
    pub fn replayed_grants(&self) -> Vec<GrantedCall> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .grants
            .values()
            .cloned()
            .collect()
    }

    /// Records a minted standing grant (issue #374).
    ///
    /// Called *before* the grant enters the live set, so the ordering failure
    /// mode is "recorded but not live" — which replay fixes — rather than "live
    /// but not recorded", which would leave a permission nobody can see or
    /// revoke.
    pub async fn record_standing_granted(&self, grant: &StandingGrant) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .standing_grants
            .insert(grant.id.clone(), grant.clone());
        self.append(&JournalRecord::StandingGrantMinted {
            grant: grant.clone(),
        })
        .await
    }

    /// Records that the operator revoked a standing grant (issue #374).
    pub async fn record_standing_revoked(
        &self,
        id: &GrantId,
        by: Actor,
        at_millis: u64,
    ) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .standing_grants
            .remove(id);
        self.append(&JournalRecord::StandingGrantRevoked {
            id: id.clone(),
            by,
            at_millis,
        })
        .await
    }

    /// Records that a standing grant reached its deadline (issue #374).
    pub async fn record_standing_expired(&self, id: &GrantId, at_millis: u64) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .standing_grants
            .remove(id);
        self.append(&JournalRecord::StandingGrantExpired {
            id: id.clone(),
            at_millis,
        })
        .await
    }

    /// Every standing grant still live according to the journal, with anything
    /// already past its deadline folded out (issue #374).
    ///
    /// The expiry filter matters beyond tidiness: the sweep only runs while the
    /// process is up, so a host that was down across a grant's deadline has no
    /// `StandingGrantExpired` line for it. Replaying on `at_millis` alone would
    /// hand a lapsed permission back to the live set, and a restart would be a
    /// way to resurrect one — the exact silent accumulation this issue forbids.
    pub fn replayed_standing_grants(&self, now_millis: u64) -> Vec<StandingGrant> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .standing_grants
            .values()
            .filter(|g| g.is_live_at(now_millis))
            .cloned()
            .collect()
    }

    /// A snapshot of the currently parked approvals, oldest first.
    pub fn pending(&self) -> Vec<PendingApproval> {
        let state = self.state.lock().expect("journal state poisoned");
        let mut out: Vec<PendingApproval> = state
            .parked
            .iter()
            .map(|(id, parked)| PendingApproval {
                id: id.clone(),
                effect: parked.effect.clone(),
                at_millis: parked.at_millis,
                task: parked.task.clone(),
                thread: parked.thread.clone(),
            })
            .collect();
        out.sort_by(|a, b| {
            a.at_millis
                .cmp(&b.at_millis)
                .then_with(|| a.id.as_ref().cmp(b.id.as_ref()))
        });
        out
    }

    /// Appends one record, whole, and does not return until the write syscall
    /// has completed.
    ///
    /// **The guarantee is process-crash durability, not host-crash durability.**
    /// There is no `sync_data`/`sync_all` here, so the bytes are in the kernel's
    /// page cache rather than on stable storage when this returns: killing the
    /// process cannot lose them, but a host crash or power loss between the
    /// append and the flush still can. The at-most-once contract therefore holds
    /// against a process dying — the case this codebase actually handles, and
    /// the one the boot reaper and the interrupted-run sweep are built around —
    /// and not against losing the machine. Whether an `fsync` per append is
    /// worth its cost on this path is a separate decision (see #392).
    ///
    /// Delegates to [`append_line`](crate::store::fs::append_line), which emits
    /// the record **and** its newline in a single blocking `write_all` under
    /// `O_APPEND`. This used to open a `tokio::fs::File` and write the two
    /// halves separately, then drop the handle without flushing — and tokio's
    /// async `File` returns from `write_all` once the write is *queued* on a
    /// blocking task, not once it lands. Measured on this code, 199 of 200
    /// appends returned with their bytes still in flight. Two consequences, both
    /// live:
    ///
    /// * The queued newline could be overtaken by the next append's opening
    ///   brace, putting two records on one physical line — the
    ///   `serde_json` "trailing characters" failure this issue was filed for.
    /// * `Ok(())` meant "queued", not "written". A commit that `record_executed`
    ///   had already reported durable could still be lost to a crash, and an
    ///   `ENOSPC` on the real write was never reported to anyone. The
    ///   at-most-once guarantee rests on that record being on disk *before* the
    ///   side effect runs, so this was the more serious half.
    ///
    /// Identical to the corruption PR #43 removed from `store::fs::append_line`
    /// and the feedback store's copy of it; the journal was the last twin.
    async fn append(&self, record: &JournalRecord) -> Result<()> {
        let line = serde_json::to_string(record)?;
        let _guard = self.write_lock.lock().await;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| self.io_err_at(parent, e))?;
        }
        append_line(&self.path, &line).await
    }

    fn io_err(&self, source: std::io::Error) -> OpenCompanyError {
        self.io_err_at(&self.path, source)
    }

    fn io_err_at(&self, path: &Path, source: std::io::Error) -> OpenCompanyError {
        OpenCompanyError::StoreIo {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Parses one journal line into the record or records it holds.
///
/// The healthy answer is one record. A line written by a pre-#386 host may hold
/// **two or more** with nothing between them, because `append` used to emit a
/// record and its newline as separate unflushed writes and the newline could
/// lose the race. `serde_json`'s stream deserializer reads concatenated values
/// natively, so such a line replays *in full* instead of being dropped — which
/// matters because dropping one would silently un-commit an `EffectExecuted`
/// key and let an at-most-once effect run a second time. Recovering the merge
/// is not a nicety; it is the difference between a cosmetic repair and a
/// duplicated payment.
///
/// A line that is truncated rather than merged — a crash partway through a
/// write, a filesystem that lost a tail — has no valid parse and is reported.
/// All-or-nothing per line: half a line applied is worse than none, because the
/// caller would have no way to know which half it got.
fn replay_line(line: &str) -> std::result::Result<Vec<JournalRecord>, String> {
    let single = match serde_json::from_str::<JournalRecord>(line) {
        Ok(record) => return Ok(vec![record]),
        Err(e) => e,
    };
    match serde_json::Deserializer::from_str(line)
        .into_iter::<JournalRecord>()
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(records) if !records.is_empty() => Ok(records),
        // Report the single-value error, not the stream one: it is the error
        // that describes the line as it was meant to be written.
        _ => Err(single.to_string()),
    }
}

impl std::fmt::Debug for RuntimeJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeJournal")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use crate::ports::now_millis;
    use crate::ports::types::EffectGroup;

    fn effect() -> Effect {
        Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        }
    }

    /// A private directory for one test's journal file.
    ///
    /// The name comes from the OS, not from [`crate::ports::generate_id`] —
    /// minted ids are unique only within a process, so two test processes
    /// sharing `/tmp` could otherwise land on the same journal path and mix
    /// their records into one file. Since #386 that no longer produces an
    /// unparseable line, but it still produces a journal holding another
    /// test's history, which fails these assertions just as thoroughly.
    /// Dropping the returned handle removes the directory, including after a
    /// failed assert.
    fn tmp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-journal-")
            .tempdir()
            .expect("tempdir")
    }

    /// An executed effect as journaled (issue #351): irreversible, against
    /// `t-1`, unless a test says otherwise.
    fn executed(at_millis: u64) -> ExecutedEffect {
        ExecutedEffect {
            kind: "filing.submit".into(),
            amount_usd: None,
            task_id: Some("t-1".into()),
            at_millis,
            irreversible: true,
        }
    }

    #[tokio::test]
    async fn effect_key_commits_once_and_survives_reload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        assert!(!journal.is_executed("cyc:0"));
        journal.record_executed("cyc:0", executed(0)).await.unwrap();
        assert!(journal.is_executed("cyc:0"));
        // Re-committing the same key does not append a second record.
        journal.record_executed("cyc:0", executed(0)).await.unwrap();

        // A fresh journal over the same file (a restart) replays the commit.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(reloaded.is_executed("cyc:0"));

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(raw.lines().filter(|l| !l.trim().is_empty()).count(), 1);

        // The re-commit is also what keeps the description list free of
        // duplicates: one key, one entry, however many times it is committed.
        assert_eq!(reloaded.irreversible_effects("t-1").len(), 1);
    }

    /// **Issue #351**: the executed record says what ran, for which card, and
    /// whether it can be taken back — and survives a restart.
    #[tokio::test]
    async fn executed_effects_are_filtered_by_task_and_by_irreversibility() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        // This card's irreversible effect — the one a retry must name.
        journal
            .record_executed("cyc:0", executed(1_000))
            .await
            .unwrap();
        // The same card, but a read: it changed nothing, so it warns about
        // nothing.
        journal
            .record_executed(
                "cyc:1",
                ExecutedEffect {
                    kind: "web.search".into(),
                    irreversible: false,
                    ..executed(1_100)
                },
            )
            .await
            .unwrap();
        // Another card's payment. Irreversible, and none of this card's
        // business.
        journal
            .record_executed(
                "cyc:2",
                ExecutedEffect {
                    kind: "payment.send".into(),
                    amount_usd: Some(2_400.0),
                    task_id: Some("t-2".into()),
                    ..executed(1_200)
                },
            )
            .await
            .unwrap();
        // A workflow delivery: no card behind it at all.
        journal
            .record_executed(
                "cyc:3",
                ExecutedEffect {
                    task_id: None,
                    ..executed(1_300)
                },
            )
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();

        let mine = reloaded.irreversible_effects("t-1");
        assert_eq!(mine.len(), 1, "{mine:?}");
        assert_eq!(mine[0].kind, "filing.submit");
        assert_eq!(mine[0].at_millis, 1_000);

        let theirs = reloaded.irreversible_effects("t-2");
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].amount_usd, Some(2_400.0));

        assert!(reloaded.irreversible_effects("t-never-ran").is_empty());
    }

    /// A journal line written before #351 carries a key and nothing else. It
    /// must still replay as an executed key — the at-most-once guarantee is not
    /// negotiable — and simply contribute no description.
    #[tokio::test]
    async fn a_pre_351_executed_line_still_replays_as_a_committed_key() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        tokio::fs::write(
            &path,
            "{\"record\":\"EffectExecuted\",\"key\":\"cyc-old:0\"}\n",
        )
        .await
        .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal.load().await.expect("a pre-#351 line still replays");
        assert!(
            journal.is_executed("cyc-old:0"),
            "dropping the key would re-run an effect that already fired",
        );
        assert!(journal.irreversible_effects("t-1").is_empty());
        assert!(
            journal.has_undescribed_history(),
            "an empty list here is 'cannot say', not 'nothing happened'",
        );
    }

    /// The companion assertion: a journal whose every executed line carries a
    /// description reports no gap, so an empty list stays a genuine all-clear
    /// and Retry stays one click.
    #[tokio::test]
    async fn a_fully_described_journal_reports_no_undescribed_history() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        journal.record_executed("cyc:0", executed(0)).await.unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(!reloaded.has_undescribed_history());
    }

    /// **Issue #351**: an operator-approved *tool call* is settled by minting a
    /// grant, never by `execute_effect_once`, so redeeming that grant is the
    /// only line in the journal that can say the call fired. It must reach the
    /// same per-task read the native path does, and survive a restart.
    #[tokio::test]
    async fn a_redeemed_grant_names_what_it_did_against_its_card() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-tool");

        journal
            .record_parked(
                &id,
                &effect(),
                1_000,
                TaskLink::Task { id: "t-1".into() },
                None,
                None,
            )
            .await
            .unwrap();
        journal.record_resolved(&id).await.unwrap();
        journal
            .record_grant_consumed(
                &id,
                Some(ExecutedEffect {
                    kind: "composio_execute".into(),
                    amount_usd: Some(2_400.0),
                    ..executed(1_200)
                }),
            )
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let named = reloaded.irreversible_effects("t-1");
        assert_eq!(named.len(), 1, "{named:?}");
        assert_eq!(named[0].kind, "composio_execute");
        assert_eq!(named[0].amount_usd, Some(2_400.0));
        assert!(
            reloaded.replayed_grants().is_empty(),
            "describing the redemption must not re-arm it",
        );
    }

    /// A redemption the runtime could not describe still journals, and simply
    /// contributes no warning — the same additive degradation a pre-#351
    /// `EffectExecuted` line has.
    #[tokio::test]
    async fn an_undescribed_redemption_still_journals_and_warns_about_nothing() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        journal
            .record_grant_consumed(&ApprovalId::new("appr-old"), None)
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(reloaded.irreversible_effects("t-1").is_empty());
    }

    /// Issue #351: the description a redeemed grant is built from comes off the
    /// park record, retained past resolution and **scrubbed of its payload** —
    /// this map outlives the queue entry, and the retry read never wants a
    /// recipient or a body.
    #[tokio::test]
    async fn an_approvals_effect_outlives_it_without_its_payload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-1");
        let parked = Effect {
            payload: serde_json::json!({ "to": "someone@example.com", "body": "secret" }),
            ..effect()
        };

        journal
            .record_parked(&id, &parked, 1_000, TaskLink::Unlinked, None, None)
            .await
            .unwrap();
        journal.record_resolved(&id).await.unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();

        // Live and replayed must agree: a grant can be redeemed either side of
        // a restart.
        for from in [&journal, &reloaded] {
            let kept = from.approval_effect(&id).expect("retained past resolve");
            assert_eq!(kept.kind, "filing.submit");
            assert_eq!(kept.group, EffectGroup::Sign);
            assert_eq!(
                kept.payload,
                serde_json::Value::Null,
                "the payload must not be retained past the queue entry",
            );
        }
        assert_eq!(journal.approval_effect(&ApprovalId::new("never")), None);
    }

    /// An approve-with-edit supersedes the park: the grant is minted against the
    /// amended arguments, so the amended amount is the one to report.
    #[tokio::test]
    async fn an_amendment_supersedes_the_parked_effect_as_the_description() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-1");

        journal
            .record_parked(
                &id,
                &Effect {
                    amount_usd: Some(2_400.0),
                    ..effect()
                },
                1_000,
                TaskLink::Unlinked,
                None,
                None,
            )
            .await
            .unwrap();
        journal
            .record_amended(
                &id,
                &Effect {
                    amount_usd: Some(400.0),
                    ..effect()
                },
                1_100,
            )
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(
            reloaded.approval_effect(&id).and_then(|e| e.amount_usd),
            Some(400.0),
            "reporting the pre-edit amount would name a payment nobody approved",
        );
    }

    #[tokio::test]
    async fn parked_approvals_rehydrate_and_resolve() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-1");
        journal
            .record_parked(&id, &effect(), now_millis(), TaskLink::Unlinked, None, None)
            .await
            .unwrap();
        assert_eq!(journal.pending().len(), 1);

        // Reload from disk: the parked approval comes back.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(reloaded.pending().len(), 1);
        assert_eq!(reloaded.pending()[0].id, id);

        // Resolving removes it, and the removal is durable.
        reloaded.record_resolved(&id).await.unwrap();
        assert!(reloaded.pending().is_empty());

        let after = RuntimeJournal::new(&path);
        after.load().await.unwrap();
        assert!(after.pending().is_empty());
    }

    /// **Issue #333**: the board task an approval was parked for is carried on
    /// the record, survives a restart, and outlives the resolution.
    ///
    /// The whole point of the field is the *resolved* case — a task's Approvals
    /// tab has to say which sign-offs were its own long after they left the
    /// queue — so the origin assertion after `record_resolved` is the one that
    /// matters, not the pending one before it.
    #[tokio::test]
    async fn a_parked_approval_carries_its_task_across_a_restart_and_a_resolution() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let mine = ApprovalId::new("appr-mine");
        let theirs = ApprovalId::new("appr-theirs");
        let orphan = ApprovalId::new("appr-orphan");
        journal
            .record_parked(
                &mine,
                &effect(),
                1_000,
                TaskLink::Task { id: "t-1".into() },
                None,
                None,
            )
            .await
            .unwrap();
        journal
            .record_parked(
                &theirs,
                &effect(),
                1_100,
                TaskLink::Task { id: "t-2".into() },
                None,
                None,
            )
            .await
            .unwrap();
        // No card behind it (a workflow delivery, an operator-chat turn).
        journal
            .record_parked(&orphan, &effect(), 1_200, TaskLink::Unlinked, None, None)
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let pending = reloaded.pending();
        assert_eq!(
            pending
                .iter()
                .filter(|p| p.task.as_ref().and_then(TaskLink::task_id) == Some("t-1"))
                .count(),
            1,
            "the parked queue must name the task, not just the effect",
        );

        // The resolution drains the queue but must not drain the link.
        reloaded.record_resolved(&mine).await.unwrap();
        assert!(reloaded.pending().iter().all(|p| p.id != mine));
        let origins = reloaded.approval_origins();
        assert_eq!(
            origins.get(&mine),
            Some(&ApprovalOrigin {
                at_millis: 1_000,
                kind: "filing.submit".into(),
                task: Some(TaskLink::Task { id: "t-1".into() }),
                run_id: None,
                thread: None,
                cycle: None,
            }),
        );
        assert_eq!(
            origins.get(&theirs).and_then(|o| o.task.clone()),
            Some(TaskLink::Task { id: "t-2".into() }),
            "a second task's approval keeps its own id, so neither absorbs the other",
        );
        // Recorded as deliberately unlinked — *not* as a missing link, which is
        // what tells the read side never to fall back to the run window for it.
        assert_eq!(
            origins.get(&orphan).and_then(|o| o.task.clone()),
            Some(TaskLink::Unlinked),
        );
    }

    /// A journal line written before #333 has no `task` key at all. It must
    /// replay with **no link** rather than failing to parse — and that absence
    /// is what the read side falls back to the old run-window correlation for.
    ///
    /// The distinction this pins is the one the whole feature rests on: a
    /// missing key replays as `None`, while a park this host recorded as having
    /// no card behind it replays as `Some(Unlinked)`. Both are "no task id",
    /// and they must not be confused.
    #[tokio::test]
    async fn a_pre_333_parked_line_replays_with_no_task() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let legacy = serde_json::json!({
            "record": "ApprovalParked",
            "id": "appr-legacy",
            "effect": effect(),
            "at_millis": 4_000,
        });
        tokio::fs::write(&path, format!("{legacy}\n"))
            .await
            .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal.load().await.expect("a pre-#333 line still replays");
        let id = ApprovalId::new("appr-legacy");
        assert_eq!(journal.pending().len(), 1);
        assert_eq!(journal.pending()[0].task, None, "no key means no link");
        assert_eq!(
            journal.approval_origins().get(&id).map(|o| o.at_millis),
            Some(4_000),
        );
        assert_eq!(journal.approval_task(&id), Some(None));

        // A park this host records with no card behind it is a *different*
        // fact, written explicitly, and must not read back as the legacy shape.
        let fresh = ApprovalId::new("appr-new");
        journal
            .record_parked(&fresh, &effect(), 5_000, TaskLink::Unlinked, None, None)
            .await
            .unwrap();
        assert_eq!(
            journal.approval_task(&fresh),
            Some(Some(TaskLink::Unlinked))
        );

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let fresh_line = raw
            .lines()
            .find(|l| l.contains("appr-new"))
            .expect("the new park was appended");
        assert!(
            fresh_line.contains(r#""link":"unlinked""#),
            "an unlinked park must say so on disk: {fresh_line}",
        );
    }

    /// A park line written before #379 has no `thread` key. It must replay as
    /// "no thread" rather than failing to parse — which is what leaves every
    /// already-parked approval on the Approvals page and in no channel, exactly
    /// as it was before this shipped.
    ///
    /// The second half is the one that has to keep working after the resolution:
    /// the thread is read off the retained origin, so it survives the queue
    /// removal. That is what lets a follow-up cycle's own re-park stay in the
    /// channel the first sign-off was asked in.
    #[tokio::test]
    async fn a_pre_379_parked_line_replays_with_no_thread_and_a_stamped_one_survives_resolution() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let legacy = serde_json::json!({
            "record": "ApprovalParked",
            "id": "appr-legacy",
            "effect": effect(),
            "at_millis": 4_000,
            "task": { "link": "unlinked" },
        });
        tokio::fs::write(&path, format!("{legacy}\n"))
            .await
            .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal.load().await.expect("a pre-#379 line still replays");
        let legacy_id = ApprovalId::new("appr-legacy");
        assert_eq!(journal.pending().len(), 1);
        assert_eq!(
            journal.pending()[0].thread,
            None,
            "no key means no conversation owns it",
        );
        assert_eq!(journal.approval_thread(&legacy_id), Some(None));
        assert_eq!(
            journal.approval_task(&legacy_id),
            Some(Some(TaskLink::Unlinked)),
            "the #333 link is untouched by the new field",
        );

        // A park stamped with the desk channel that produced it.
        let stamped = ApprovalId::new("appr-desk");
        journal
            .record_parked(
                &stamped,
                &effect(),
                5_000,
                TaskLink::Unlinked,
                Some("desk-finance".to_string()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            journal
                .pending()
                .iter()
                .find(|p| p.id == stamped)
                .unwrap()
                .thread,
            Some("desk-finance".to_string()),
        );

        // Resolving drains the queue but must not drain the origin thread —
        // the follow-up cycle reads it back from here.
        journal.record_resolved(&stamped).await.unwrap();
        assert!(journal.pending().iter().all(|p| p.id != stamped));
        assert_eq!(
            journal.approval_thread(&stamped),
            Some(Some("desk-finance".to_string())),
        );

        // And it round-trips through a reload, from the raw line.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let stamped_line = raw
            .lines()
            .find(|l| l.contains("appr-desk"))
            .expect("the stamped park was appended");
        assert!(
            stamped_line.contains(r#""thread":"desk-finance""#),
            "a thread-stamped park must say so on disk: {stamped_line}",
        );
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(
            reloaded.approval_thread(&stamped),
            Some(Some("desk-finance".to_string())),
        );
        assert_eq!(reloaded.approval_thread(&legacy_id), Some(None));
    }

    #[tokio::test]
    async fn expired_record_removes_parked_and_survives_reload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-exp");
        journal
            .record_parked(&id, &effect(), now_millis(), TaskLink::Unlinked, None, None)
            .await
            .unwrap();
        assert_eq!(journal.pending().len(), 1);

        journal.record_expired(&id, now_millis()).await.unwrap();
        assert!(journal.pending().is_empty());

        // A restart replays the expiry: the approval stays gone.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(reloaded.pending().is_empty());

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("ApprovalExpired"));
    }

    #[tokio::test]
    async fn amended_record_is_audit_only_and_round_trips() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-amend");
        journal
            .record_parked(&id, &effect(), now_millis(), TaskLink::Unlinked, None, None)
            .await
            .unwrap();

        let mut amended = effect();
        amended.payload = serde_json::json!({ "edited": true });
        journal
            .record_amended(&id, &amended, now_millis())
            .await
            .unwrap();
        // The audit record alone does not drain the queue.
        assert_eq!(journal.pending().len(), 1);
        // The paired resolution removes it.
        journal.record_resolved(&id).await.unwrap();
        assert!(journal.pending().is_empty());

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(reloaded.pending().is_empty());

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("ApprovalAmended"));
        assert!(raw.contains("\"edited\":true"));
    }

    /// Issue #305: the park instant outlives the parked entry.
    ///
    /// Waiting time is only recoverable by joining a resolved approval back to
    /// when it parked, and the event log carries no park time. If the index were
    /// cleared alongside `parked` on resolve — the obvious symmetry — every
    /// *finished* wait would be unreadable, which is exactly the case the header
    /// needs. Expiry (the default-deny path) must retain it for the same reason.
    #[tokio::test]
    async fn approval_origins_outlive_resolution_and_expiry_and_survive_reload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let resolved = ApprovalId::new("appr-resolved");
        let expired = ApprovalId::new("appr-expired");
        journal
            .record_parked(&resolved, &effect(), 1_000, TaskLink::Unlinked, None, None)
            .await
            .unwrap();
        journal
            .record_parked(&expired, &effect(), 2_000, TaskLink::Unlinked, None, None)
            .await
            .unwrap();

        journal.record_resolved(&resolved).await.unwrap();
        journal.record_expired(&expired, 9_000).await.unwrap();

        // Both left the queue...
        assert!(journal.pending().is_empty());
        // ...but their park instants are still joinable.
        let origins = journal.approval_origins();
        assert_eq!(origins.get(&resolved).map(|o| o.at_millis), Some(1_000));
        assert_eq!(origins.get(&expired).map(|o| o.at_millis), Some(2_000));

        // And a restart replays them out of the file, so history predating this
        // process is readable too.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let origins = reloaded.approval_origins();
        assert!(reloaded.pending().is_empty());
        assert_eq!(origins.get(&resolved).map(|o| o.at_millis), Some(1_000));
        assert_eq!(origins.get(&expired).map(|o| o.at_millis), Some(2_000));
    }

    fn grant(id: &str, at_millis: u64) -> GrantedCall {
        GrantedCall {
            approval_id: ApprovalId::new(id),
            agent: "finance".into(),
            tool: "composio_execute".into(),
            args: serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" }),
            at_millis,
            origin_thread: None,
        }
    }

    /// Issue #243: a grant minted before a restart is still redeemable after it.
    ///
    /// The window between "operator approved" and "agent re-issued the call"
    /// spans a model turn, so a deploy or crash inside it is ordinary. Without
    /// replay the operator's approval would evaporate and the agent would come
    /// back asking for the same permission it had just been given.
    #[tokio::test]
    async fn a_live_grant_replays_across_a_restart() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_granted(&grant("appr-1", 1_000))
            .await
            .unwrap();
        assert_eq!(journal.replayed_grants().len(), 1);

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let replayed = reloaded.replayed_grants();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].approval_id, ApprovalId::new("appr-1"));
        assert_eq!(replayed[0].agent, "finance");
        assert_eq!(replayed[0].tool, "composio_execute");
        assert_eq!(
            replayed[0].args,
            serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" }),
            "the exact arguments the operator approved survive the restart"
        );
    }

    /// The other half, and the one that actually matters for safety: a grant
    /// that already fired — or that lapsed and was announced as lapsed — must
    /// NOT come back on replay.
    ///
    /// A single-use grant resurrected by a restart is no longer single-use. The
    /// fold therefore *removes* on both terminal records, unlike `origins`
    /// (#305) which deliberately retains.
    #[tokio::test]
    async fn consumed_and_expired_grants_are_not_rehydrated() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_granted(&grant("consumed", 1_000))
            .await
            .unwrap();
        journal
            .record_granted(&grant("expired", 2_000))
            .await
            .unwrap();
        journal.record_granted(&grant("live", 3_000)).await.unwrap();
        assert_eq!(journal.replayed_grants().len(), 3);

        journal
            .record_grant_consumed(&ApprovalId::new("consumed"), None)
            .await
            .unwrap();
        journal
            .record_grant_expired(&ApprovalId::new("expired"), 9_000)
            .await
            .unwrap();

        let still_live: Vec<_> = journal.replayed_grants();
        assert_eq!(still_live.len(), 1);
        assert_eq!(still_live[0].approval_id, ApprovalId::new("live"));

        // And the removal is durable, not just in-memory.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let replayed = reloaded.replayed_grants();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].approval_id, ApprovalId::new("live"));

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("ApprovalGranted"));
        assert!(raw.contains("GrantConsumed"));
        assert!(raw.contains("GrantExpired"));
    }

    fn standing(id: &str, tool: &str, expires_at_millis: u64) -> StandingGrant {
        StandingGrant {
            id: GrantId::new(id),
            agent: "ops".into(),
            tool: tool.into(),
            granted_by: Actor {
                kind: crate::ports::types::ActorKind::User,
                id: "user-42".into(),
            },
            approval_id: ApprovalId::new(format!("appr-{id}")),
            at_millis: 1_000,
            expires_at_millis,
            origin_thread: None,
            scope: None,
        }
    }

    /// Issue #374: a standing grant survives a restart, with its expiry and the
    /// operator who granted it intact.
    #[tokio::test]
    async fn a_standing_grant_replays_across_a_restart() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_standing_granted(&standing("g1", "shell", 100_000))
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let replayed = reloaded.replayed_standing_grants(2_000);
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].id, GrantId::new("g1"));
        assert_eq!(replayed[0].tool, "shell");
        assert_eq!(replayed[0].expires_at_millis, 100_000);
        assert_eq!(
            replayed[0].granted_by.id, "user-42",
            "who opened this tool up is the point of the record"
        );
    }

    /// Revoked, expired, and *silently lapsed* standing grants all stay gone.
    ///
    /// The third case is the one only replay can catch: the sweep runs while the
    /// process is up, so a host that was down across a deadline never wrote a
    /// `StandingGrantExpired` line. Replaying on the record alone would hand the
    /// permission back, making a restart a way to resurrect one.
    #[tokio::test]
    async fn revoked_expired_and_lapsed_standing_grants_are_not_rehydrated() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        for g in [
            standing("revoked", "shell", 100_000),
            standing("expired", "workspace_write", 100_000),
            standing("lapsed", "web_fetch", 3_000),
            standing("live", "shell", 100_000),
        ] {
            journal.record_standing_granted(&g).await.unwrap();
        }

        journal
            .record_standing_revoked(
                &GrantId::new("revoked"),
                Actor {
                    kind: crate::ports::types::ActorKind::User,
                    id: "user-42".into(),
                },
                5_000,
            )
            .await
            .unwrap();
        journal
            .record_standing_expired(&GrantId::new("expired"), 5_000)
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        // `lapsed` has no terminal record at all — only its deadline stops it.
        let replayed = reloaded.replayed_standing_grants(10_000);
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].id, GrantId::new("live"));

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("StandingGrantMinted"));
        assert!(raw.contains("StandingGrantRevoked"));
        assert!(raw.contains("StandingGrantExpired"));
    }

    /// A journal written before #374 decodes unchanged, and replays no standing
    /// grants. The forward-only half — an old binary cannot read a new journal —
    /// is the same contract every prior variant addition made.
    #[tokio::test]
    async fn a_pre_374_journal_decodes_and_yields_no_standing_grants() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_parked(
                &ApprovalId::new("appr-old"),
                &effect(),
                500,
                TaskLink::Unlinked,
                None,
                None,
            )
            .await
            .unwrap();
        journal
            .record_granted(&grant("appr-old", 1_000))
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(reloaded.pending().len(), 1);
        assert_eq!(
            reloaded.replayed_grants().len(),
            1,
            "the single-use path replays byte-identically"
        );
        assert!(reloaded.replayed_standing_grants(2_000).is_empty());
    }

    /// The grant records must not disturb the approval-queue fold they share a
    /// file with — including #309's origin index, which the Task Detail
    /// waiting-time read joins against.
    #[tokio::test]
    async fn grant_records_leave_the_parked_queue_and_origins_intact() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let parked_id = ApprovalId::new("appr-parked");
        journal
            .record_parked(&parked_id, &effect(), 500, TaskLink::Unlinked, None, None)
            .await
            .unwrap();
        journal
            .record_granted(&grant("appr-granted", 1_000))
            .await
            .unwrap();
        journal
            .record_grant_consumed(&ApprovalId::new("appr-granted"), None)
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(
            reloaded.pending().len(),
            1,
            "the parked approval is untouched"
        );
        assert_eq!(reloaded.pending()[0].id, parked_id);
        assert_eq!(
            reloaded
                .approval_origins()
                .get(&parked_id)
                .map(|o| o.at_millis),
            Some(500)
        );
        assert!(reloaded.replayed_grants().is_empty());
    }

    /// Every non-empty line of the journal at `path`, parsed. Panics with the
    /// offending line's number and text when one does not parse, because a
    /// torn line is exactly what these tests exist to catch and
    /// `unwrap`-on-`Err` hides which line it was.
    async fn parse_every_line(path: &Path) -> Vec<JournalRecord> {
        let raw = tokio::fs::read_to_string(path).await.expect("journal file");
        raw.lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, line)| {
                serde_json::from_str::<JournalRecord>(line)
                    .unwrap_or_else(|e| panic!("line {} did not parse: {e}\n  {line}", i + 1))
            })
            .collect()
    }

    /// **Issue #386**: rapid appends through a *single* journal must not tear a
    /// line.
    ///
    /// This is the shape CI actually hit. `append` used to leave its trailing
    /// newline in a `tokio::fs::File` whose background write nobody awaited,
    /// then drop the handle and release the lock — so the next append's opening
    /// bytes could reach the file before the previous record's terminator, and
    /// two records landed on one line. One writer was enough; concurrency
    /// across instances was never required.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rapid_appends_through_one_journal_never_tear_a_line() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        const N: usize = 256;
        for i in 0..N {
            journal
                .record_executed(&format!("cyc:{i}"), executed(i as u64))
                .await
                .unwrap();
        }

        let records = parse_every_line(&path).await;
        assert_eq!(records.len(), N, "every append is its own line");

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        for i in 0..N {
            assert!(
                reloaded.is_executed(&format!("cyc:{i}")),
                "cyc:{i} must survive the reload",
            );
        }
    }

    /// **Issue #386**: a line an old host merged replays in full.
    ///
    /// This is the shape already sitting in journals written before the write
    /// fix, and the shape CI tripped over. It must not be *skipped*: dropping a
    /// merged line would un-commit an `EffectExecuted` key and let an
    /// at-most-once effect fire again, which is a worse outcome than the parse
    /// error it replaces.
    #[tokio::test]
    async fn a_merged_line_replays_every_record_it_holds() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        let merged = format!(
            "{}{}",
            serde_json::to_string(&JournalRecord::EffectExecuted {
                key: "cyc:0".into(),
                effect: Some(executed(0)),
            })
            .unwrap(),
            serde_json::to_string(&JournalRecord::EffectExecuted {
                key: "cyc:1".into(),
                effect: Some(executed(1)),
            })
            .unwrap(),
        );
        let intact = serde_json::to_string(&JournalRecord::EffectExecuted {
            key: "cyc:2".into(),
            effect: Some(executed(2)),
        })
        .unwrap();
        tokio::fs::write(&path, format!("{merged}\n{intact}\n"))
            .await
            .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal
            .load()
            .await
            .expect("a merged line must not fail the load");
        for key in ["cyc:0", "cyc:1", "cyc:2"] {
            assert!(journal.is_executed(key), "{key} must replay");
        }
        assert!(
            journal.corruption().is_empty(),
            "a merged line is recovered, not lost, so it is not corruption",
        );
    }

    /// **Issue #386**: a truncated line is reported, and the records around it
    /// still replay.
    ///
    /// The old `load` returned `Err` here, which failed the company's boot: one
    /// unreadable line cost every readable one after it, plus the console an
    /// operator would need to repair the file.
    #[tokio::test]
    async fn a_truncated_line_is_reported_and_the_rest_still_replays() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        let record = |key: &str, at| {
            serde_json::to_string(&JournalRecord::EffectExecuted {
                key: key.into(),
                effect: Some(executed(at)),
            })
            .unwrap()
        };
        let whole = record("cyc:1", 1);
        let truncated = &whole[..whole.len() / 2];
        tokio::fs::write(
            &path,
            format!(
                "{}\n{truncated}\n{}\n",
                record("cyc:0", 0),
                record("cyc:2", 2)
            ),
        )
        .await
        .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal
            .load()
            .await
            .expect("one bad line must not fail the boot");

        assert!(journal.is_executed("cyc:0"), "the line before must replay");
        assert!(
            journal.is_executed("cyc:2"),
            "the lines after the damage are the ones the old load lost",
        );
        assert!(
            !journal.is_executed("cyc:1"),
            "the truncated record is gone"
        );

        let corruption = journal.corruption();
        assert_eq!(corruption.len(), 1, "exactly one line was unreadable");
        assert_eq!(corruption[0].line, 2, "the report must locate the line");
        assert_eq!(corruption[0].bytes, truncated.len());
        assert!(
            !corruption[0].message.contains("filing.submit"),
            "a corruption report must not quote the line's contents",
        );
    }

    /// **Issue #386**: a torn write can split a multi-byte codepoint, so the
    /// damaged line is not merely bad JSON — it is not valid UTF-8 at all.
    ///
    /// `load` used to `read_to_string`, which fails on the first invalid byte
    /// anywhere in the file. That turned exactly the damage this recovery path
    /// exists for into the whole-boot failure it exists to prevent, and no
    /// amount of per-line JSON handling downstream could have saved it. Raised
    /// in review of PR #389.
    #[tokio::test]
    async fn a_line_that_is_not_valid_utf8_is_skipped_like_any_other_damage() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        let record = |key: &str, at| {
            serde_json::to_string(&JournalRecord::EffectExecuted {
                key: key.into(),
                effect: Some(executed(at)),
            })
            .unwrap()
        };

        // A lone continuation byte: never valid on its own, which is what the
        // tail of a split codepoint looks like.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(record("cyc:0", 0).as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0x7b, 0x9f, 0x8d]);
        bytes.push(b'\n');
        bytes.extend_from_slice(record("cyc:2", 2).as_bytes());
        bytes.push(b'\n');
        tokio::fs::write(&path, &bytes).await.unwrap();

        let journal = RuntimeJournal::new(&path);
        journal
            .load()
            .await
            .expect("invalid UTF-8 on one line must not fail the boot");

        assert!(journal.is_executed("cyc:0"), "the line before must replay");
        assert!(
            journal.is_executed("cyc:2"),
            "the lines after the damage must still replay",
        );

        let corruption = journal.corruption();
        assert_eq!(corruption.len(), 1, "exactly one line was unreadable");
        assert_eq!(corruption[0].line, 2, "the report must locate the line");
    }

    /// **Issue #386**: when `append` returns, the record is on the file.
    ///
    /// The deterministic half of the bug, and the more serious one. The
    /// at-most-once guarantee is that an effect's key is durable *before* the
    /// side effect runs; the old write path returned once the write was queued
    /// on tokio's blocking pool, so `record_executed` reported a commit that a
    /// crash could still lose and an `ENOSPC` on the real write reached nobody.
    /// Measured against that path, 199 of 200 appends failed this assertion —
    /// the torn line was the rare, visible symptom of a window that was open
    /// almost always.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_append_has_reached_the_file_before_it_returns() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let mut expected = 0usize;
        for i in 0..64u64 {
            let key = format!("cyc:{i}");
            expected += serde_json::to_string(&JournalRecord::EffectExecuted {
                key: key.clone(),
                effect: Some(executed(i)),
            })
            .unwrap()
            .len()
                + 1;
            journal.record_executed(&key, executed(i)).await.unwrap();
            // A synchronous stat, so the assertion cannot be satisfied by the
            // very blocking pool that would still be running a queued write.
            let on_disk = std::fs::metadata(&path).expect("journal file").len() as usize;
            assert_eq!(
                on_disk,
                expected,
                "append #{} returned with {} of {expected} bytes on the file",
                i + 1,
                on_disk,
            );
        }
    }

    /// **Issue #386**: two journals over one path must not interleave.
    ///
    /// `write_lock` is per-instance, so it serialises nothing between two
    /// `RuntimeJournal` values sharing a file. Nothing in the type stops a
    /// caller building two, and the test suite builds them routinely. The
    /// defence is the process-wide per-path lock plus the single whole-line
    /// write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appends_from_two_journals_over_one_path_lose_nothing() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        const N: usize = 128;
        let one = Arc::new(RuntimeJournal::new(&path));
        let two = Arc::new(RuntimeJournal::new(&path));

        let a = tokio::spawn({
            let one = Arc::clone(&one);
            async move {
                for i in 0..N {
                    one.record_executed(&format!("a:{i}"), executed(i as u64))
                        .await
                        .unwrap();
                }
            }
        });
        let b = tokio::spawn({
            let two = Arc::clone(&two);
            async move {
                for i in 0..N {
                    two.record_executed(&format!("b:{i}"), executed(i as u64))
                        .await
                        .unwrap();
                }
            }
        });
        a.await.unwrap();
        b.await.unwrap();

        let records = parse_every_line(&path).await;
        assert_eq!(records.len(), N * 2, "no record may be lost or merged");

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        for i in 0..N {
            assert!(reloaded.is_executed(&format!("a:{i}")), "a:{i} lost");
            assert!(reloaded.is_executed(&format!("b:{i}")), "b:{i} lost");
        }
    }

    #[test]
    fn expired_and_amended_records_round_trip_under_record_tag() {
        for record in [
            JournalRecord::ApprovalExpired {
                id: ApprovalId::new("x"),
                at_millis: 42,
            },
            JournalRecord::ApprovalAmended {
                id: ApprovalId::new("y"),
                amended_effect: effect(),
                at_millis: 7,
            },
            JournalRecord::ApprovalGranted {
                grant: grant("z", 11),
            },
            JournalRecord::GrantConsumed {
                id: ApprovalId::new("z"),
                effect: None,
            },
            JournalRecord::GrantConsumed {
                id: ApprovalId::new("z2"),
                effect: Some(executed(21)),
            },
            JournalRecord::GrantExpired {
                id: ApprovalId::new("z"),
                at_millis: 13,
            },
        ] {
            let json = serde_json::to_value(&record).unwrap();
            assert!(json.get("record").is_some());
            let back: JournalRecord = serde_json::from_value(json).unwrap();
            // Re-serialize to compare (JournalRecord has no PartialEq).
            assert_eq!(
                serde_json::to_string(&back).unwrap(),
                serde_json::to_string(&record).unwrap()
            );
        }
    }
}
