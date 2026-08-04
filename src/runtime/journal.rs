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
use std::sync::Mutex as StdMutex;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as TokioMutex;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::types::{ApprovalId, Effect};
use crate::runtime::grants::GrantedCall;

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
        /// The board task whose dispatch cycle parked this effect, when it was
        /// parked inside one.
        ///
        /// Not read for the queue itself — it is what lets the **approval
        /// follow-up cycle** know which card it is finishing (issue #351).
        /// Under `supervised`, an irreversible effect never executes in the
        /// cycle that emitted it: it parks, and the operator's approval starts
        /// a fresh cycle whose only event is `ApprovalResolved`. Without this
        /// field that cycle knows no task, so every effect executed the way the
        /// policy intends would be attributed to nothing and named on no retry
        /// dialog.
        ///
        /// Skipped when serializing and defaulted when absent, so journal lines
        /// written before this field existed replay as `None` rather than
        /// failing to parse.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
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

/// In-memory state rebuilt from (and kept in sync with) `journal.jsonl`.
#[derive(Default)]
struct State {
    executed: HashSet<String>,
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
    parked: HashMap<ApprovalId, (Effect, u64)>,
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
    /// The board task each approval was parked for, retained after it leaves
    /// `parked` (issue #351).
    ///
    /// Read by the approval **follow-up** cycle, which knows only an
    /// [`ApprovalId`]: it is what lets the effect an operator just approved be
    /// attributed to the card that asked for it. Never removed, for the same
    /// reason [`park_instants`](Self::park_instants) is not — the join happens
    /// after the queue entry is gone, and holds only entries that have a task.
    approval_tasks: HashMap<ApprovalId, String>,
    /// When each approval was *parked*, retained after it leaves `parked`.
    ///
    /// This is what makes waiting time readable (issue #305). The park instant
    /// is journal-only — [`CompanyEvent::ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved)
    /// carries the resolution but no park time — so a resolved approval's wait
    /// is only recoverable by joining the two on [`ApprovalId`]. Entries are
    /// therefore **never removed** on resolve or expiry: the index has the same
    /// append-only lifetime as the file it is replayed from, and costs one
    /// `(id, u64)` per approval ever parked.
    park_instants: HashMap<ApprovalId, u64>,
    /// Grants minted and not yet consumed or expired (issue #243).
    ///
    /// Unlike [`park_instants`](Self::park_instants) this one IS removed from on
    /// the terminal records: a replayed grant is handed straight back to the
    /// live [`GrantSet`](crate::runtime::grants::GrantSet), so keeping a
    /// consumed or expired entry here would re-arm a tool call that already ran
    /// (or that the operator was already told had lapsed) on every restart.
    grants: HashMap<ApprovalId, GrantedCall>,
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
/// Exactly one process may write a given journal file. [`append`](Self::append)
/// emits the record and its newline as two separate writes under an in-process
/// lock, so a second writer on the same path can interleave between them and
/// leave two records on one line, which then fails to parse on replay.
pub struct RuntimeJournal {
    path: PathBuf,
    state: StdMutex<State>,
    write_lock: TokioMutex<()>,
}

impl RuntimeJournal {
    /// Opens (or prepares) the journal at `path` without loading it.
    ///
    /// Call [`load`](Self::load) to replay an existing journal into memory.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            state: StdMutex::new(State::default()),
            write_lock: TokioMutex::new(()),
        }
    }

    /// Replays the on-disk journal into memory, reconstructing the executed-key
    /// set and the parked-approval queue. Idempotent.
    pub async fn load(&self) -> Result<()> {
        let contents = match tokio::fs::read_to_string(&self.path).await {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(self.io_err(e)),
        };

        let mut state = State::default();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalRecord>(line)? {
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
                    task_id,
                } => {
                    state.park_instants.insert(id.clone(), at_millis);
                    if let Some(task_id) = task_id {
                        state.approval_tasks.insert(id.clone(), task_id);
                    }
                    state.retain_approval_effect(&id, &effect);
                    state.parked.insert(id, (effect, at_millis));
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
            }
        }
        *self.state.lock().expect("journal state poisoned") = state;
        Ok(())
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

    /// Records a newly parked approval, tagged with the board task whose cycle
    /// parked it (issue #351) — `None` when no card is behind it.
    pub async fn record_parked(
        &self,
        id: &ApprovalId,
        effect: &Effect,
        at_millis: u64,
        task_id: Option<&str>,
    ) -> Result<()> {
        let task_id = task_id.map(str::to_string);
        {
            let mut state = self.state.lock().expect("journal state poisoned");
            state.park_instants.insert(id.clone(), at_millis);
            if let Some(task_id) = task_id.clone() {
                state.approval_tasks.insert(id.clone(), task_id);
            }
            state.retain_approval_effect(id, effect);
            state.parked.insert(id.clone(), (effect.clone(), at_millis));
        }
        self.append(&JournalRecord::ApprovalParked {
            id: id.clone(),
            effect: effect.clone(),
            at_millis,
            task_id,
        })
        .await
    }

    /// The board task an approval was parked for, if any (issue #351).
    ///
    /// Answers the approval follow-up cycle's only question: the operator
    /// approved *this* id — whose card was that?
    pub fn approval_task(&self, id: &ApprovalId) -> Option<String> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .approval_tasks
            .get(id)
            .cloned()
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

    /// A snapshot of when each approval was parked, keyed by [`ApprovalId`],
    /// including approvals that have since been resolved or expired.
    ///
    /// The read side joins this against the event log's
    /// [`ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved) to
    /// recover how long an approval was actually waiting (issue #305). Taken as
    /// one snapshot per request rather than per lookup, so a fold never holds
    /// the state lock while it works.
    pub fn park_instants(&self) -> HashMap<ApprovalId, u64> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .park_instants
            .clone()
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

    /// A snapshot of the currently parked approvals, oldest first.
    pub fn pending(&self) -> Vec<PendingApproval> {
        let state = self.state.lock().expect("journal state poisoned");
        let mut out: Vec<PendingApproval> = state
            .parked
            .iter()
            .map(|(id, (effect, at_millis))| PendingApproval {
                id: id.clone(),
                effect: effect.clone(),
                at_millis: *at_millis,
            })
            .collect();
        out.sort_by(|a, b| {
            a.at_millis
                .cmp(&b.at_millis)
                .then_with(|| a.id.as_ref().cmp(b.id.as_ref()))
        });
        out
    }

    async fn append(&self, record: &JournalRecord) -> Result<()> {
        let line = serde_json::to_string(record)?;
        let _guard = self.write_lock.lock().await;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| self.io_err_at(parent, e))?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|e| self.io_err(e))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| self.io_err(e))?;
        file.write_all(b"\n").await.map_err(|e| self.io_err(e))?;
        Ok(())
    }

    /// The file this journal appends to.
    ///
    /// Test-only, and deliberately so: nothing in the runtime needs the path,
    /// but a test pinning replay of a record shaped the way an older build wrote
    /// it has to write that record itself.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
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

impl std::fmt::Debug for RuntimeJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeJournal")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod test {
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
        }
    }

    /// A private directory for one test's journal file.
    ///
    /// The name comes from the OS, not from [`crate::ports::generate_id`] —
    /// minted ids are unique only within a process, so two test processes
    /// sharing `/tmp` could otherwise land on the same journal path and
    /// interleave their appends into an unparseable line. Dropping the
    /// returned handle removes the directory, including after a failed assert.
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
            .record_parked(&id, &effect(), 1_000, Some("t-1"))
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
            .record_parked(&id, &parked, 1_000, None)
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
            .record_parked(&id, &effect(), now_millis(), None)
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

    /// Issue #351: the card an approval was parked for outlives the queue
    /// entry, because the cycle that needs it runs *after* the resolution.
    #[tokio::test]
    async fn an_approvals_task_survives_its_resolution_and_a_restart() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let linked = ApprovalId::new("appr-linked");
        let orphan = ApprovalId::new("appr-orphan");
        journal
            .record_parked(&linked, &effect(), 1_000, Some("t-1"))
            .await
            .unwrap();
        journal
            .record_parked(&orphan, &effect(), 1_100, None)
            .await
            .unwrap();
        journal.record_resolved(&linked).await.unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(
            reloaded.approval_task(&linked),
            Some("t-1".into()),
            "the follow-up cycle looks this up after the queue entry is gone",
        );
        assert_eq!(reloaded.approval_task(&orphan), None);
        assert_eq!(reloaded.approval_task(&ApprovalId::new("never")), None);
    }

    #[tokio::test]
    async fn expired_record_removes_parked_and_survives_reload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-exp");
        journal
            .record_parked(&id, &effect(), now_millis(), None)
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
            .record_parked(&id, &effect(), now_millis(), None)
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
    async fn park_instants_outlive_resolution_and_expiry_and_survive_reload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let resolved = ApprovalId::new("appr-resolved");
        let expired = ApprovalId::new("appr-expired");
        journal
            .record_parked(&resolved, &effect(), 1_000, None)
            .await
            .unwrap();
        journal
            .record_parked(&expired, &effect(), 2_000, None)
            .await
            .unwrap();

        journal.record_resolved(&resolved).await.unwrap();
        journal.record_expired(&expired, 9_000).await.unwrap();

        // Both left the queue...
        assert!(journal.pending().is_empty());
        // ...but their park instants are still joinable.
        let instants = journal.park_instants();
        assert_eq!(instants.get(&resolved), Some(&1_000));
        assert_eq!(instants.get(&expired), Some(&2_000));

        // And a restart replays them out of the file, so history predating this
        // process is readable too.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let instants = reloaded.park_instants();
        assert!(reloaded.pending().is_empty());
        assert_eq!(instants.get(&resolved), Some(&1_000));
        assert_eq!(instants.get(&expired), Some(&2_000));
    }

    fn grant(id: &str, at_millis: u64) -> GrantedCall {
        GrantedCall {
            approval_id: ApprovalId::new(id),
            agent: "finance".into(),
            tool: "composio_execute".into(),
            args: serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" }),
            at_millis,
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
    /// fold therefore *removes* on both terminal records, unlike `park_instants`
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

    /// The grant records must not disturb the approval-queue fold they share a
    /// file with — including #309's `park_instants` index, which the Task Detail
    /// waiting-time read joins against.
    #[tokio::test]
    async fn grant_records_leave_the_parked_queue_and_park_instants_intact() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let parked_id = ApprovalId::new("appr-parked");
        journal
            .record_parked(&parked_id, &effect(), 500, None)
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
        assert_eq!(reloaded.park_instants().get(&parked_id), Some(&500));
        assert!(reloaded.replayed_grants().is_empty());
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
