//! The brain's compressed traces and task results, over [`Bound`].

use async_trait::async_trait;

use super::bound::Bound;
use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::{CompanyId, CompressedTrace, EvictionPolicy, MemoryStore, TaskResult};
use crate::runtime::maintenance::TRACE_RETENTION_LIMIT;

/// The brain's compressed traces and task results.
///
/// The spec sequences this port last, and says it may reasonably never move:
/// append-only, eviction-driven trace rows are the shape the contract suits
/// least. It is here because leaving one port on a different backend would mean
/// the export bundle spans two engines.
///
/// The gap this closes is `evict`. The contract has no archive tier and no bulk
/// delete by predicate, so eviction is a **move** between two namespaces — see
/// [`ProviderMemoryStore::evict`].
pub struct ProviderMemoryStore {
    traces: Bound,
    archive: Bound,
    task_results: Bound,
}

impl ProviderMemoryStore {
    pub(in crate::store::memory) fn new(
        traces: Bound,
        archive: Bound,
        task_results: Bound,
    ) -> Self {
        Self {
            traces,
            archive,
            task_results,
        }
    }

    /// Reads the live trace set, oldest first.
    async fn ordered_traces(&self, company: &CompanyId) -> Result<Vec<CompressedTrace>> {
        let mut traces: Vec<CompressedTrace> = self.traces.list(company).await?;
        // Total order, not just by timestamp: two traces stamped in the same
        // millisecond must not reorder between reads, or `recent_traces` returns
        // a different window each call and eviction evicts a different set.
        traces.sort_by(|a, b| {
            a.at_millis
                .cmp(&b.at_millis)
                .then_with(|| a.cycle_id.cmp(&b.cycle_id))
        });
        Ok(traces)
    }

    /// Reads the archived trace set, for the operator's inspection.
    ///
    /// The archive is a bounded recovery tier on this facade: eviction moves
    /// traces here rather than destroying them. The export path carries this
    /// tier separately from the live `GET /memory/traces` window — both read
    /// distinct namespaces. This accessor exists so the operator tier and the
    /// "archives rather than destroys" property tests can observe the tier itself.
    pub(in crate::store::memory) async fn archived_traces(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<CompressedTrace>> {
        self.archive.list(company).await
    }

    /// Restores traces directly into the archive tier.
    pub(in crate::store::memory) async fn restore_archived_traces(
        &self,
        company: &CompanyId,
        traces: &[CompressedTrace],
    ) -> Result<()> {
        for trace in traces {
            self.archive
                .put(company, &trace.cycle_id, trace, "trace")
                .await?;
        }
        Ok(())
    }

    /// Bounds the archive tier to the newest `n` archived traces.
    ///
    /// Eviction moves traces OUT of the live window rather than destroying
    /// them; without a matching cap here the archive would retain every trace a
    /// company ever evicted, so the documented retention policy would bound the
    /// inspectable window but not storage. Keeping the newest `n` evicted
    /// traces bounds the tier at `n` and total trace storage at `2n` — the live
    /// window plus the eviction history nearest to it.
    async fn prune_archive(&self, id: &CompanyId, n: usize) -> Result<()> {
        if n == 0 {
            let archived = self.archive.list::<CompressedTrace>(id).await?;
            for trace in archived {
                self.archive.forget(id, &trace.cycle_id).await?;
            }
            return Ok(());
        }
        let mut archived = self.archive.list::<CompressedTrace>(id).await?;
        if archived.len() <= n {
            return Ok(());
        }
        // Same total order as the live set, so "newest" is unambiguous even
        // when two traces share a millisecond.
        archived.sort_by(|a, b| {
            a.at_millis
                .cmp(&b.at_millis)
                .then_with(|| a.cycle_id.cmp(&b.cycle_id))
        });
        let prune = archived.len() - n;
        for trace in archived.into_iter().take(prune) {
            self.archive.forget(id, &trace.cycle_id).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl MemoryStore for ProviderMemoryStore {
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()> {
        self.traces.put(id, &trace.cycle_id, &trace, "trace").await
    }

    async fn recent_traces(&self, id: &CompanyId, limit: usize) -> Result<Vec<CompressedTrace>> {
        // Avoid even touching the provider when the caller requests no rows.
        // This matters for the provider-backed facade because `list` has no
        // limit argument and otherwise decodes the entire trace partition.
        if limit == 0 {
            return Ok(Vec::new());
        }
        let traces = self.ordered_traces(id).await?;
        // Newest last, per the port contract, so the tail is the window.
        let skip = traces.len().saturating_sub(limit);
        Ok(traces.into_iter().skip(skip).collect())
    }

    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()> {
        self.task_results
            .put(id, &result.task_id, &result, "task-result")
            .await
    }

    /// Evicts per `policy`, **archiving rather than destroying**.
    ///
    /// `docs/spec/company-brain/memory.md` makes this normative: "evicted traces
    /// are archived, not deleted, until retention policy or the Operator says
    /// otherwise". The contract offers no archive tier, so the behaviour lives
    /// here as a move between two namespaces.
    ///
    /// Order matters and is not arbitrary: the archive write happens **before**
    /// the live delete. There is no transaction spanning two provider calls, so
    /// one of the two orders has to be chosen for what it does when the process
    /// dies in between. Archive-then-delete leaves a trace in both places — a
    /// duplicate the next read reconciles. Delete-then-archive loses it. For a
    /// port whose whole promise is "not destroyed", that asymmetry decides it.
    ///
    /// The returned count is **traces this call removed from the live set**, not
    /// traces archived. Those differ when `forget` reports a key was already
    /// gone: the archive write has happened by then, so the archive can hold an
    /// entry this call did not remove. That is a concurrent eviction having got
    /// there first, and the entry is archived either way — which is the
    /// behaviour the port promises. Reporting it as removed *here* would be the
    /// lie, so the count stays narrow.
    ///
    /// The same asymmetry appears if a `put` or `forget` fails mid-loop: the
    /// error propagates and the traces already processed stay archived. That is
    /// the archive-then-delete order behaving as designed under partial failure
    /// — a duplicate the next read reconciles, never a loss. What must NOT be
    /// skipped on that path is the archive bound itself: traces already moved
    /// by the failed pass are still in the archive, so the prune below runs
    /// before the error propagates, keeping the tier at its limit even when a
    /// maintenance pass repeatedly fails partway.
    ///
    /// Every eviction additionally bounds the archive itself to the newest
    /// `n` evicted traces (see [`ProviderMemoryStore::prune_archive`]): a
    /// `KeepRecent { n }` eviction bounds it to `n`, and `OlderThan` — which
    /// has no `n` of its own — to the retention limit, so the policy that
    /// bounds the live window also bounds storage on every path: a company
    /// that runs for years does not accumulate every trace it ever evicted
    /// beside the 32 it keeps, and an operator-sized `OlderThan` sweep cannot
    /// grow the archive without bound. That bound is what keeps
    /// `GET /memory/archives` a bounded read by construction rather than a
    /// download of the whole archive followed by a discard.
    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64> {
        let traces = self.ordered_traces(id).await?;
        let doomed: Vec<CompressedTrace> = match &policy {
            EvictionPolicy::KeepRecent { n } => {
                let keep_from = traces.len().saturating_sub(*n);
                traces.into_iter().take(keep_from).collect()
            }
            EvictionPolicy::OlderThan { before_millis } => traces
                .into_iter()
                .filter(|trace| trace.at_millis < *before_millis)
                .collect(),
        };
        let mut evicted = 0u64;
        let move_result = (async {
            for trace in doomed {
                self.archive
                    .put(id, &trace.cycle_id, &trace, "trace")
                    .await?;
                if self.traces.forget(id, &trace.cycle_id).await? {
                    evicted += 1;
                }
            }
            Ok::<(), OpenCompanyError>(())
        })
        .await;
        // Bound the archive on every eviction path — the partial-failure path
        // included. `KeepRecent` prunes to its own `n`; `OlderThan` has no `n`
        // to bound by, so it prunes to the retention limit — the same window
        // the live set is held to, which is what keeps the tier "the eviction
        // history nearest to the live window" and the archive read bounded for
        // any policy.
        let bound = match policy {
            EvictionPolicy::KeepRecent { n } => n,
            EvictionPolicy::OlderThan { .. } => TRACE_RETENTION_LIMIT,
        };
        if let Err(move_err) = move_result {
            // A provider failure mid-loop still leaves the traces already
            // moved sitting in the archive, and a maintenance pass that keeps
            // failing partway must not grow the tier past its bound across
            // retries. Prune best-effort, then report the failure that
            // actually happened.
            if let Err(prune_err) = self.prune_archive(id, bound).await {
                tracing::warn!(
                    error = %prune_err,
                    "archive prune failed after a partial eviction failure; the archive may exceed \
                     its retention bound"
                );
            }
            return Err(move_err);
        }
        self.prune_archive(id, bound).await?;
        Ok(evicted)
    }
}
