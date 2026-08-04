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
        /// description, exactly as they behaved before.
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

/// A side effect that **actually ran** (issue #351): what it was, which board
/// task it was run for, and whether it is one that cannot be taken back.
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
    /// Every effect that ran and said what it was, oldest first (issue #351).
    ///
    /// Append-only for the same reason [`executed`](Self::executed) is: an
    /// effect that fired stays fired, and a retry warning that forgot half the
    /// history would be worse than none. One small record per executed effect,
    /// with no payload — see [`ExecutedEffect`].
    executed_effects: Vec<ExecutedEffect>,
    parked: HashMap<ApprovalId, (Effect, u64)>,
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
                    // description simply does not exist to replay.
                    if let Some(effect) = effect {
                        state.executed_effects.push(effect);
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
                    state.parked.insert(id, (effect, at_millis));
                }
                JournalRecord::ApprovalResolved { id } => {
                    state.parked.remove(&id);
                }
                JournalRecord::ApprovalExpired { id, .. } => {
                    state.parked.remove(&id);
                }
                // Audit-only: the paired `ApprovalResolved` handles removal.
                JournalRecord::ApprovalAmended { .. } => {}
                JournalRecord::ApprovalGranted { grant } => {
                    state.grants.insert(grant.approval_id.clone(), grant);
                }
                JournalRecord::GrantConsumed { id } => {
                    state.grants.remove(&id);
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
            state.executed_effects.push(effect.clone());
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

    /// The irreversible effects this task has already executed, oldest first
    /// (issue #351).
    ///
    /// Drawn from the journal's own executed record — the same append-only set
    /// that makes effects at-most-once — rather than re-derived from timeline
    /// labels, which describe what an agent *said* and not what actually fired.
    pub fn irreversible_effects(&self, task_id: &str) -> Vec<ExecutedEffect> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .executed_effects
            .iter()
            .filter(|e| e.irreversible && e.task_id.as_deref() == Some(task_id))
            .cloned()
            .collect()
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
    pub async fn record_grant_consumed(&self, id: &ApprovalId) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .grants
            .remove(id);
        self.append(&JournalRecord::GrantConsumed { id: id.clone() })
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
            .record_grant_consumed(&ApprovalId::new("consumed"))
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
            .record_grant_consumed(&ApprovalId::new("appr-granted"))
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
