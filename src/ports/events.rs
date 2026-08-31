//! The [`EventLog`] port: append-only, replayable event history.
//!
//! # Retention
//!
//! The journal is append-only and grows for the life of a company (issue #275).
//! [`EventLog::prune`] is the only operation that removes an entry, and it is
//! deliberately awkward to invoke by accident:
//!
//! - It is **never** called on the append path. The [`UsageMeter`] precedent
//!   evicts on write against a hard-coded window; a journal must not, because
//!   the journal is what export/import ships and what boot replays. Retention
//!   here is something an operator asks for, not something a write does.
//! - It takes an explicit [`RetentionPolicy`], and the [`Default`] policy
//!   removes nothing. A caller that forgets to configure a bound gets a no-op,
//!   not a silent deletion.
//! - It refuses to touch an event whose kind is
//!   [`RetentionClass::Permanent`], and classification is an exhaustive match
//!   (see [`CompanyEvent::retention_class`]) so a newly added event variant
//!   fails the build until somebody decides. Unclassifiable means permanent.
//! - The backend does not decide *what* goes. [`plan_prune`] is a pure
//!   function shared by all three production backends, so fs, sqlite and
//!   mongodb cannot drift apart on the one operation that cannot be undone.
//!
//! Sequence numbers are **not** renumbered. Pruning leaves gaps, which
//! `read_from`'s `>= seq` scan already tolerates: a cursor or SSE subscriber
//! parked on a pruned sequence resumes at the next surviving one. Renumbering
//! would instead invalidate every stored cross-reference — thread parents,
//! reaction targets, and the `TaskDiscussionRedacted` tombstone from #358 all
//! address a message by its sequence.
//!
//! [`UsageMeter`]: crate::ports::usage::UsageMeter

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, StoredEvent};
use crate::{OpenCompanyError, Result};

/// Whether a journal entry may ever be discarded by a retention pass.
///
/// The split is not "important vs unimportant" — it is "reconstructible
/// operational exhaust vs a record something else depends on". Two things make
/// an entry [`Permanent`](Self::Permanent):
///
/// 1. **It is the audit trail.** An approval verdict, a lifecycle transition, a
///    payment, a tool-access grant. Deleting these is the failure mode the
///    issue's first question guards against.
/// 2. **Another entry addresses it by sequence.** A thread reply points at its
///    parent, a reaction points at a message, and `TaskDiscussionRedacted`
///    points at the post it supersedes. Pruning a referent leaves a dangling
///    pointer that no fold can repair, so every referent kind is permanent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionClass {
    /// Operational exhaust: safe to discard once a bound says so.
    Prunable,
    /// Audit-bearing, or addressed by sequence from another entry. Never
    /// removed, whatever the policy says.
    Permanent,
}

/// An explicit bound on how much of the journal is kept.
///
/// Both bounds are `None` by default, and a policy with no bound set removes
/// nothing — retention is opt-in per caller, never an ambient default. When
/// both are set an entry goes if *either* bound selects it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Discard prunable entries older than this, measured back from the newest
    /// entry in the log rather than from wall-clock now. `None` disables the
    /// age bound.
    ///
    /// Anchoring to the newest *observed* entry is the same rule
    /// [`retention_cutoff`](crate::ports::usage::retention_cutoff) uses for
    /// usage samples: it makes a pass deterministic and testable with no clock,
    /// and it never empties a dormant company's journal just because time
    /// passed with nothing happening.
    pub max_age_millis: Option<u64>,
    /// Keep at most this many prunable entries *of each kind*, discarding the
    /// oldest beyond it. `None` disables the count bound.
    ///
    /// Per-kind rather than per-log, mirroring OpenHuman's
    /// `MAX_FLOW_RUNS_PER_FLOW`: a burst of webhook traffic should not evict
    /// the run outcomes the issue set out to bound.
    pub max_entries_per_kind: Option<usize>,
}

impl RetentionPolicy {
    /// The policy that removes nothing. Identical to [`Default`], named for
    /// call sites that want to say so out loud.
    pub fn keep_everything() -> Self {
        Self::default()
    }

    /// A policy bounding prunable entries by age.
    pub fn with_max_age_millis(millis: u64) -> Self {
        Self {
            max_age_millis: Some(millis),
            ..Self::default()
        }
    }

    /// A policy bounding prunable entries by per-kind count.
    pub fn with_max_entries_per_kind(entries: usize) -> Self {
        Self {
            max_entries_per_kind: Some(entries),
            ..Self::default()
        }
    }

    /// Whether this policy can remove anything at all.
    ///
    /// Backends short-circuit on this, so a default-constructed policy costs
    /// one branch rather than a full scan.
    pub fn is_noop(&self) -> bool {
        self.max_age_millis.is_none() && self.max_entries_per_kind.is_none()
    }

    /// The oldest `at_millis` this policy keeps, given the newest entry the log
    /// holds. Entries strictly older than the cutoff are discarded; an entry
    /// exactly `max_age_millis` old is inside the window and kept.
    ///
    /// `None` when no age bound is set.
    pub fn cutoff_millis(&self, newest_at_millis: u64) -> Option<u64> {
        self.max_age_millis
            .map(|age| newest_at_millis.saturating_sub(age))
    }
}

/// What one [`EventLog::prune`] pass did.
///
/// Returned rather than logged so a caller can assert on it, surface it, or
/// refuse to proceed — a deletion that reports nothing is indistinguishable
/// from a deletion that went wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// How many entries the pass considered.
    pub scanned: usize,
    /// How many entries it removed.
    pub removed: usize,
    /// The lowest sequence still present afterwards, or `None` if the log is
    /// empty. Gaps below it are expected and permanent.
    pub oldest_retained: Option<EventSeq>,
}

/// One item from an [`EventLog`] live subscription.
///
/// A lag notice is deliberately not a [`StoredEvent`]: it was never written to
/// the journal, has no sequence, and says only that a live receiver fell behind.
/// Consumers must discard incremental assumptions and re-read durable state.
#[derive(Clone, Debug, PartialEq)]
// `Event` is deliberately stored by value rather than boxed: these items are
// produced one at a time by a live event stream and consumed immediately —
// never collected into an owning container — so the large_enum_variant
// optimization (boxing the larger arm to shrink the enum) would only add a
// heap allocation to the per-event hot path without any size benefit.
#[allow(clippy::large_enum_variant)]
pub enum EventStreamItem {
    /// One durable event appended after the subscription opened.
    Event(StoredEvent),
    /// The receiver fell behind by this many broadcast messages.
    Gap { missed: u64 },
}

/// Decides which entries a retention pass removes.
///
/// Pure and total: no clock, no I/O, no backend knowledge. Every production
/// backend routes its `prune` through this, which is what makes "fs, sqlite and
/// mongodb agree" a property of the code rather than of three parallel test
/// suites.
///
/// `events` may arrive in any order; the returned sequences are sorted
/// ascending and contain no duplicates.
///
/// Three rules, applied in order:
///
/// 1. A no-op policy removes nothing.
/// 2. A [`RetentionClass::Permanent`] entry is never removed.
/// 3. The highest-sequence entry is never removed, whatever its class.
///
/// Rule 3 is not cosmetic. `FsEventLog` and `SqliteStore` both allocate the
/// next sequence from the highest one present, so a pass that emptied the log
/// would hand the next append a sequence already used by a *retained*
/// cross-reference — silently re-pointing a redaction tombstone or a thread
/// parent at an unrelated message. Keeping the watermark entry makes sequence
/// reuse unreachable without asking either backend to persist a counter it
/// does not have today.
pub fn plan_prune(events: &[StoredEvent], policy: &RetentionPolicy) -> Vec<EventSeq> {
    if policy.is_noop() || events.is_empty() {
        return Vec::new();
    }

    let watermark = events.iter().map(|e| e.seq).max();
    let newest_at = events.iter().map(|e| e.at_millis).max().unwrap_or_default();
    let cutoff = policy.cutoff_millis(newest_at);

    // Candidates are prunable and not the sequence watermark. Ordered newest
    // first so the per-kind count bound keeps the most recent entries.
    let mut candidates: Vec<&StoredEvent> = events
        .iter()
        .filter(|e| e.event.retention_class() == RetentionClass::Prunable)
        .filter(|e| Some(e.seq) != watermark)
        .collect();
    candidates.sort_by_key(|e| std::cmp::Reverse(e.seq));

    let mut doomed = Vec::new();
    let mut seen_per_kind: HashMap<&'static str, usize> = HashMap::new();

    for event in candidates {
        let too_old = cutoff.is_some_and(|c| event.at_millis < c);

        let kept_of_kind = seen_per_kind.entry(event.event.kind()).or_insert(0);
        let over_count = policy
            .max_entries_per_kind
            .is_some_and(|max| *kept_of_kind >= max);

        if too_old || over_count {
            doomed.push(event.seq);
        } else {
            // Only surviving entries count against the per-kind budget;
            // otherwise an age-evicted entry would consume a slot that a
            // younger entry of the same kind should have had.
            *kept_of_kind += 1;
        }
    }

    doomed.sort();
    doomed
}

/// Append-only, replayable event log. Boot replays the tail to rebuild
/// in-flight state.
#[async_trait]
pub trait EventLog: Send + Sync {
    /// Appends an event, returning its assigned sequence number.
    async fn append(&self, id: &CompanyId, event: CompanyEvent) -> Result<EventSeq>;
    /// Reads up to `limit` events with sequence `>= seq`.
    async fn read_from(
        &self,
        id: &CompanyId,
        seq: EventSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>>;
    /// Reads up to `limit` events with sequence `< before`, newest first.
    ///
    /// An absent cursor means the current tail. Backends should implement this
    /// directly when they can: transcript readers open at the tail, and doing
    /// a forward `read_from(0, MAX)` merely to keep its last page turns every
    /// chat open into an ever-growing allocation. The fallback keeps custom
    /// test ports source-compatible; production stores override it.
    async fn read_before(
        &self,
        id: &CompanyId,
        before: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let mut events = self.read_from(id, EventSeq::new(0), usize::MAX).await?;
        events.retain(|event| before.is_none_or(|cursor| event.seq < cursor));
        events.reverse();
        events.truncate(limit);
        Ok(events)
    }
    /// Subscribes to events appended after the call.
    ///
    /// A [`EventStreamItem::Gap`] means the receiver missed one or more live
    /// entries. It is not persisted and carries no payload from those entries.
    fn subscribe(&self, id: &CompanyId) -> BoxStream<'static, EventStreamItem>;

    /// Applies `policy`, removing the entries [`plan_prune`] selects, and
    /// reports what went.
    ///
    /// The default refuses. A backend that has not implemented retention
    /// answers "port not implemented" rather than reporting a successful pass
    /// that removed nothing — the two are indistinguishable to a caller, and
    /// only one of them is true. Implement it, or let it fail loudly.
    ///
    /// Removal is permanent and leaves sequence gaps by design; see the module
    /// docs.
    async fn prune(&self, id: &CompanyId, policy: &RetentionPolicy) -> Result<PruneReport> {
        let _ = (id, policy);
        Err(OpenCompanyError::Unimplemented("EventLog::prune"))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::{Actor, ActorKind, WorkflowNodeStatus};

    /// A fixed base far from epoch 0 so cutoff arithmetic stays positive.
    const BASE: u64 = 1_000_000_000_000;
    const DAY: u64 = 86_400_000;

    fn run_started(n: u64) -> CompanyEvent {
        CompanyEvent::WorkflowRunStarted {
            workflow_id: "wf".into(),
            run_id: format!("run-{n}"),
            scheduled: false,
            started_by: None,
        }
    }

    fn node_finished(n: u64) -> CompanyEvent {
        CompanyEvent::WorkflowNodeFinished {
            workflow_id: "wf".into(),
            run_id: format!("run-{n}"),
            node_id: format!("node-{n}"),
            status: WorkflowNodeStatus::Ok,
            elapsed_ms: 1,
            diagnostics: Vec::new(),
            agent_run_id: None,
        }
    }

    fn lifecycle(n: u64) -> CompanyEvent {
        CompanyEvent::LifecycleChanged {
            from: "running".into(),
            to: format!("paused-{n}"),
            by: Actor {
                kind: ActorKind::Operator,
                id: "operator".into(),
            },
        }
    }

    /// Builds a log from `(seq, at_millis, event)` triples.
    fn log(entries: Vec<(u64, u64, CompanyEvent)>) -> Vec<StoredEvent> {
        entries
            .into_iter()
            .map(|(seq, at_millis, event)| StoredEvent {
                seq: EventSeq::new(seq),
                company: CompanyId::new("acme"),
                event,
                at_millis,
            })
            .collect()
    }

    fn seqs(raw: &[u64]) -> Vec<EventSeq> {
        raw.iter().copied().map(EventSeq::new).collect()
    }

    #[test]
    fn default_policy_is_a_no_op() {
        let events = log(vec![
            (0, BASE, run_started(0)),
            (1, BASE + DAY, run_started(1)),
            (2, BASE + 2 * DAY, run_started(2)),
        ]);
        assert!(RetentionPolicy::default().is_noop());
        assert_eq!(plan_prune(&events, &RetentionPolicy::default()), vec![]);
    }

    #[test]
    fn empty_log_prunes_nothing() {
        assert_eq!(
            plan_prune(&[], &RetentionPolicy::with_max_entries_per_kind(0)),
            vec![]
        );
    }

    #[test]
    fn age_bound_is_anchored_to_the_newest_entry_not_a_clock() {
        // Newest is 10 days after BASE; a 5-day window keeps everything from
        // BASE+5d onward. No wall clock is consulted, so this test is stable
        // whenever it runs.
        let events = log(vec![
            (0, BASE, run_started(0)),
            (1, BASE + 4 * DAY, run_started(1)),
            (2, BASE + 5 * DAY, run_started(2)),
            (3, BASE + 10 * DAY, run_started(3)),
        ]);
        let policy = RetentionPolicy::with_max_age_millis(5 * DAY);
        assert_eq!(plan_prune(&events, &policy), seqs(&[0, 1]));
    }

    #[test]
    fn an_entry_exactly_at_the_window_edge_is_kept() {
        let events = log(vec![
            (0, BASE, run_started(0)),
            (1, BASE + 5 * DAY, run_started(1)),
        ]);
        // Newest is BASE+5d, so the cutoff is exactly BASE. Seq 0 sits on it
        // and is inside the window.
        let policy = RetentionPolicy::with_max_age_millis(5 * DAY);
        assert_eq!(plan_prune(&events, &policy), vec![]);
    }

    #[test]
    fn permanent_kinds_survive_any_policy() {
        let events = log(vec![
            (0, BASE, lifecycle(0)),
            (1, BASE, lifecycle(1)),
            (2, BASE, lifecycle(2)),
            (3, BASE + 1000 * DAY, run_started(0)),
        ]);
        let policy = RetentionPolicy {
            max_age_millis: Some(1),
            max_entries_per_kind: Some(0),
        };
        // Only the watermark is prunable-by-kind, and rule 3 protects it.
        assert_eq!(plan_prune(&events, &policy), vec![]);
    }

    #[test]
    fn count_bound_keeps_the_newest_per_kind() {
        // Two prunable kinds interleaved: each gets its own budget of 1, so a
        // burst of one kind cannot evict the other.
        let events = log(vec![
            (0, BASE, run_started(0)),
            (1, BASE, node_finished(0)),
            (2, BASE, run_started(1)),
            (3, BASE, node_finished(1)),
            (4, BASE, run_started(2)),
            (5, BASE, lifecycle(0)),
        ]);
        let policy = RetentionPolicy::with_max_entries_per_kind(1);
        // Kept: seq 4 (newest run-start), seq 3 (newest node-finish), seq 5
        // (permanent, and the watermark).
        assert_eq!(plan_prune(&events, &policy), seqs(&[0, 1, 2]));
    }

    #[test]
    fn the_sequence_watermark_is_never_removed() {
        // Every entry is prunable and every entry is over both bounds; the
        // highest sequence still survives, because fs and sqlite allocate the
        // next sequence from it.
        let events = log(vec![
            (0, BASE, run_started(0)),
            (1, BASE, run_started(1)),
            (2, BASE, run_started(2)),
        ]);
        let policy = RetentionPolicy {
            max_age_millis: Some(0),
            max_entries_per_kind: Some(0),
        };
        assert_eq!(plan_prune(&events, &policy), seqs(&[0, 1]));
    }

    #[test]
    fn either_bound_alone_can_select_an_entry() {
        let events = log(vec![
            (0, BASE, run_started(0)),
            (1, BASE + 100 * DAY, run_started(1)),
            (2, BASE + 100 * DAY, run_started(2)),
        ]);
        // Age alone would take seq 0; count alone (keep 2) would also take
        // seq 0. Together they still take exactly seq 0 — the rule is a union,
        // not a double count.
        let policy = RetentionPolicy {
            max_age_millis: Some(DAY),
            max_entries_per_kind: Some(2),
        };
        assert_eq!(plan_prune(&events, &policy), seqs(&[0]));
    }

    #[test]
    fn an_age_evicted_entry_does_not_consume_a_count_slot() {
        // Sequence order and timestamp order disagree here — seq 1 is the old
        // one — which is the only arrangement that can tell the two counting
        // rules apart. Backfill and clock skew both produce it.
        //
        // seq 1 is evicted by age. If it *also* consumed one of the two
        // per-kind slots, seq 0 would be evicted for being over the count.
        // It must not be: the budget is for what is kept.
        let events = log(vec![
            (0, BASE + 100 * DAY, run_started(0)),
            (1, BASE, run_started(1)),
            (2, BASE + 100 * DAY, run_started(2)),
            (3, BASE + 100 * DAY, lifecycle(0)),
        ]);
        let policy = RetentionPolicy {
            max_age_millis: Some(DAY),
            max_entries_per_kind: Some(2),
        };
        assert_eq!(plan_prune(&events, &policy), seqs(&[1]));
    }

    #[test]
    fn input_order_does_not_change_the_outcome() {
        let mut events = log(vec![
            (0, BASE, run_started(0)),
            (1, BASE, run_started(1)),
            (2, BASE, run_started(2)),
            (3, BASE, lifecycle(0)),
        ]);
        let policy = RetentionPolicy::with_max_entries_per_kind(1);
        let forward = plan_prune(&events, &policy);
        events.reverse();
        assert_eq!(forward, plan_prune(&events, &policy));
        assert_eq!(forward, seqs(&[0, 1]));
    }

    #[test]
    fn chat_kinds_addressed_by_sequence_are_permanent() {
        // The referents of a thread parent, a reaction, and #358's redaction
        // tombstone. Pruning any of them dangles a stored pointer.
        for event in [
            CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                text: "hi".into(),
                by: None,
                chat: None,
                parent: None,
                deliverable: None,
                attachments: Vec::new(),
            },
            CompanyEvent::AgentReply {
                mentions: Vec::new(),
                mention_depth: 0,
                chat_id: "desk".into(),
                agent_id: "ceo".into(),
                text: "hello".into(),
                steps: vec![],
                task_id: None,
                parent: None,
            },
        ] {
            assert_eq!(
                event.retention_class(),
                RetentionClass::Permanent,
                "{} must stay permanent: other entries address it by sequence",
                event.kind()
            );
        }
    }

    #[test]
    fn kind_matches_the_serialized_tag() {
        // `kind()` is hand-written, so pin it against what serde actually
        // emits; a rename that misses one of the two would otherwise be silent.
        for event in [run_started(0), node_finished(0), lifecycle(0)] {
            let json = serde_json::to_value(&event).expect("serialize");
            assert_eq!(
                json.get("kind").and_then(|k| k.as_str()),
                Some(event.kind()),
                "kind() disagrees with the serde tag"
            );
        }
    }
}
