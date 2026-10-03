use std::sync::Arc;

use super::BoundMemory;
use super::facades::EXTERNAL_TAG;
use super::tests::{FlakyStore, a_fact, acme_id, engine, everything, globex_id, with_handle};
use crate::ports::{CompressedTrace, ContextChunk, EvictionPolicy};

#[tokio::test]
async fn facts_round_trip_through_the_provider() {
    let mem = engine();
    let facts = mem.facts();
    let id = acme_id();
    let fact = a_fact("f1", "Ships on Fridays");
    facts.upsert(&id, &fact).await.unwrap();
    let listed = facts.list(&id, None, None).await.unwrap();
    assert_eq!(listed, vec![fact]);
}

#[tokio::test]
async fn one_companys_facts_are_invisible_to_another() {
    // The single largest risk in this phase, against one shared engine.
    let mem = engine();
    mem.facts()
        .upsert(&acme_id(), &a_fact("f1", "secret"))
        .await
        .unwrap();
    let theirs = mem.facts().list(&globex_id(), None, None).await.unwrap();
    assert!(
        theirs.is_empty(),
        "a company read another's facts: {theirs:?}"
    );
}

#[tokio::test]
async fn deleting_a_fact_reports_whether_it_existed() {
    let mem = engine();
    let id = acme_id();
    mem.facts().upsert(&id, &a_fact("f1", "t")).await.unwrap();
    assert!(mem.facts().delete(&id, "f1").await.unwrap());
    assert!(!mem.facts().delete(&id, "f1").await.unwrap());
}

#[tokio::test]
async fn context_round_trips_and_peeks_a_range() {
    let mem = engine();
    let id = acme_id();
    let context = mem.context();
    let addr = context
        .put(
            &id,
            ContextChunk {
                label: "notes/one".into(),
                body: "the quick brown fox".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        context.peek(&id, &addr, None).await.unwrap(),
        "the quick brown fox"
    );
    assert_eq!(context.peek(&id, &addr, Some(4..9)).await.unwrap(), "quick");
    let listed = context.list(&id, "notes/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].label, "notes/one");
    assert!(listed[0].stored_at_millis > 0, "the stamp must be recorded");
}

#[tokio::test]
async fn scratch_is_unreachable_from_durable_recall() {
    // Asserted against the recall path itself, not against a routing table:
    // the durable facade is asked to find content that only exists in scratch.
    let mem = engine();
    let id = acme_id();
    mem.scratch()
        .put(
            &id,
            ContextChunk {
                label: "wip".into(),
                body: "provisional working-out about penguins".into(),
            },
        )
        .await
        .unwrap();

    let hits = mem.context().search(&id, "penguins", 10).await.unwrap();
    assert!(hits.is_empty(), "durable recall reached scratch: {hits:?}");
    let listed = mem.context().list(&id, "").await.unwrap();
    assert!(
        listed.is_empty(),
        "durable list reached scratch: {listed:?}"
    );

    // The agent and desk partitions are siblings of scratch too.
    assert!(
        mem.agent_context("cto")
            .search(&id, "penguins", 10)
            .await
            .unwrap()
            .is_empty()
    );

    // And it really is stored — the emptiness above is a firewall, not a
    // silently-dropped write.
    assert_eq!(mem.scratch().list(&id, "").await.unwrap().len(), 1);
}

#[tokio::test]
async fn agent_partitions_do_not_leak_into_each_other() {
    let mem = engine();
    let id = acme_id();
    mem.agent_context("cto")
        .put(
            &id,
            ContextChunk {
                label: "l".into(),
                body: "cto private note".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        mem.agent_context("cfo")
            .list(&id, "")
            .await
            .unwrap()
            .is_empty()
    );
}

/// The sibling of [`agent_partitions_do_not_leak_into_each_other`]: two
/// desks, not two agents. `desk_context` had never been driven through an
/// isolation assertion — only through the cache-bound tests, which never put
/// anything into the stores they open.
#[tokio::test]
async fn desk_partitions_do_not_leak_into_each_other() {
    let mem = engine();
    let id = acme_id();
    mem.desk_context("engineering")
        .put(
            &id,
            ContextChunk {
                label: "l".into(),
                body: "engineering private note".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        mem.desk_context("sales")
            .list(&id, "")
            .await
            .unwrap()
            .is_empty()
    );
}

/// `agent_context` and `desk_context` key their cache off `format!("agent:{id}")`
/// / `format!("desk:{id}")` — a prefix, not a separate map. An agent and a
/// desk that happen to share the same raw id string must still land in
/// different stores; a regression that dropped the prefix (or collapsed both
/// into one key space) would pass every existing test here, since none of
/// them use the same id for both an agent and a desk.
#[tokio::test]
async fn an_agent_and_a_desk_sharing_the_same_raw_id_do_not_share_a_partition() {
    let mem = engine();
    let id = acme_id();
    mem.agent_context("ops")
        .put(
            &id,
            ContextChunk {
                label: "l".into(),
                body: "the ops agent's note".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        mem.desk_context("ops")
            .list(&id, "")
            .await
            .unwrap()
            .is_empty(),
        "the ops desk must not see the ops agent's partition"
    );
}

#[tokio::test]
async fn inbound_writes_are_marked_external_and_internal_writes_are_not() {
    // Laundering external content into internal-trust content is the failure
    // the provenance mark exists to prevent.
    let (raw, memory) = with_handle();
    let id = acme_id();
    memory
        .inbound_context()
        .put(
            &id,
            ContextChunk {
                label: "web".into(),
                body: "scraped from a page".into(),
            },
        )
        .await
        .unwrap();
    memory
        .context()
        .put(
            &id,
            ContextChunk {
                label: "own".into(),
                body: "the company decided this".into(),
            },
        )
        .await
        .unwrap();

    let items = everything(&raw).await;
    let external = |needle: &str| {
        items
            .iter()
            .find(|hit| hit.text.contains(needle))
            .map(|hit| hit.meta.tags.iter().any(|tag| tag == EXTERNAL_TAG))
    };
    assert_eq!(
        external("scraped from a page"),
        Some(true),
        "an inbound-channel write must stay marked as external"
    );
    assert_eq!(
        external("the company decided this"),
        Some(false),
        "the company's own writes must not be marked external"
    );
}

#[tokio::test]
async fn recent_traces_are_newest_last_and_bounded() {
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    for (n, cycle) in ["c1", "c2", "c3", "c4", "c5"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: format!("cycle {cycle}"),
                    at_millis: 100 + n as u64,
                },
            )
            .await
            .unwrap();
    }
    let recent = memory.recent_traces(&id, 2).await.unwrap();
    assert_eq!(
        recent
            .iter()
            .map(|t| t.cycle_id.as_str())
            .collect::<Vec<_>>(),
        vec!["c4", "c5"],
        "newest last"
    );
}

#[tokio::test]
async fn evict_archives_rather_than_destroys() {
    // Normative in docs/spec/company-brain/memory.md. The contract has no
    // archive tier, so this is the decorator's own behaviour and it needs its
    // own assertion against the archive, not just against a count.
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    for (n, cycle) in ["c1", "c2", "c3", "c4", "c5"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: "s".into(),
                    at_millis: 100 + n as u64,
                },
            )
            .await
            .unwrap();
    }

    let archived = memory
        .evict(&id, EvictionPolicy::KeepRecent { n: 2 })
        .await
        .unwrap();
    assert_eq!(archived, 3);
    let live = memory.recent_traces(&id, usize::MAX).await.unwrap();
    assert_eq!(
        live.iter().map(|t| t.cycle_id.as_str()).collect::<Vec<_>>(),
        vec!["c4", "c5"]
    );

    // Eviction MOVES the traces out of the live set, and the archive is
    // bounded by the same policy: of the three evicted, the newest two (c2,
    // c3) are retained and the oldest (c1) is pruned. The archive keeps the
    // eviction history nearest to the live window, never the whole lifetime.
    let kept = mem.archived_traces(&id).await.unwrap();
    let mut ids: Vec<&str> = kept.iter().map(|t| t.cycle_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["c2", "c3"],
        "evicted traces live on, bounded at n"
    );
}

#[tokio::test]
async fn evict_keep_recent_bounds_the_archive() {
    // The retention policy must bound storage on this backend too, not just
    // the inspectable live window: a company that cycles for years would
    // otherwise accumulate every trace it ever evicted in the archive.
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    // 100 cycles in batches of 10, evicting down to 8 after each batch.
    for batch in 0..10 {
        for n in 0..10 {
            let i = batch * 10 + n;
            memory
                .save_trace(
                    &id,
                    CompressedTrace {
                        cycle_id: format!("c{i}"),
                        summary: "s".into(),
                        at_millis: 100 + i as u64,
                    },
                )
                .await
                .unwrap();
        }
        memory
            .evict(&id, EvictionPolicy::KeepRecent { n: 8 })
            .await
            .unwrap();
    }
    let live = memory.recent_traces(&id, usize::MAX).await.unwrap();
    assert_eq!(live.len(), 8, "the live window stays bounded");
    let archived = mem.archived_traces(&id).await.unwrap();
    assert!(
        archived.len() <= 8,
        "the archive stays bounded at n too: {} traces retained from 100 cycles",
        archived.len()
    );
}

#[tokio::test]
async fn evict_older_than_archives_everything_it_removes() {
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    for (n, cycle) in ["c1", "c2", "c3"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: "s".into(),
                    at_millis: 100 + n as u64,
                },
            )
            .await
            .unwrap();
    }
    let removed = memory
        .evict(
            &id,
            EvictionPolicy::OlderThan {
                before_millis: u64::MAX,
            },
        )
        .await
        .unwrap();
    assert_eq!(removed, 3);
    assert!(
        memory
            .recent_traces(&id, usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        mem.archived_traces(&id).await.unwrap().len(),
        3,
        "none lost"
    );
}

#[tokio::test]
async fn evict_older_than_bounds_the_archive_at_the_retention_limit() {
    // `OlderThan` has no `n` of its own to bound the archive by, so eviction
    // prunes it to the retention limit — the same window the live set is held
    // to. Without this, an operator-sized older-than sweep would move the whole
    // lifetime into the archive and grow storage without bound, and
    // `GET /memory/archives` would be a bounded read only by the route's
    // sort-and-tail rather than by construction.
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    let limit = crate::runtime::maintenance::TRACE_RETENTION_LIMIT;
    let n = limit + 10;
    for i in 0..n {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: format!("c{i}"),
                    summary: "s".into(),
                    at_millis: 100 + i as u64,
                },
            )
            .await
            .unwrap();
    }
    let removed = memory
        .evict(
            &id,
            EvictionPolicy::OlderThan {
                before_millis: u64::MAX,
            },
        )
        .await
        .unwrap();
    assert_eq!(removed, n as u64, "the whole live window is evicted");
    assert!(
        memory
            .recent_traces(&id, usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        mem.archived_traces(&id).await.unwrap().len(),
        limit,
        "an older-than sweep cannot grow the archive past the retention limit"
    );
}

#[tokio::test]
async fn evict_bounds_the_archive_when_the_loop_fails_partway() {
    // The archive bound must survive a partial eviction failure. Archive-
    // then-delete has already moved the traces before the failure, so
    // skipping the prune on the error path would let a maintenance pass that
    // repeatedly fails partway grow the archive past its retention limit. The
    // prune is best-effort there: the original provider error still
    // propagates.
    //
    // The flaky engine fails the 6th store into the archive partition. The
    // first eviction archives four traces (calls 1-4) and prunes to n=1; the
    // second archives one trace (call 5), then fails on the next (call 6) —
    // leaving c5 and c4 in the archive, which the failure-path prune must
    // bring back down to the newest one (c5).
    let mem = BoundMemory::bind(FlakyStore::arc(6));
    let id = acme_id();
    let memory = mem.memory();
    for (n, cycle) in ["c1", "c2", "c3", "c4", "c5"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: "s".into(),
                    at_millis: 100 + n as u64,
                },
            )
            .await
            .unwrap();
    }
    // First pass: fully successful, leaves the archive bounded at 1.
    let archived = memory
        .evict(&id, EvictionPolicy::KeepRecent { n: 1 })
        .await
        .unwrap();
    assert_eq!(archived, 4);

    for (n, cycle) in ["c6", "c7", "c8"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: "s".into(),
                    at_millis: 105 + n as u64,
                },
            )
            .await
            .unwrap();
    }
    // Second pass: archives c5 (call 5), then fails on c6 (call 6).
    let err = memory
        .evict(&id, EvictionPolicy::KeepRecent { n: 1 })
        .await
        .expect_err("the injected archive store failure must propagate");
    assert!(
        err.to_string().contains("injected archive store failure"),
        "the original provider error must survive: {err}"
    );

    // The trace moved before the failure is still bounded: the archive keeps
    // the newest evicted trace (c5) and prunes the one it displaced (c4).
    let kept = mem.archived_traces(&id).await.unwrap();
    let ids: Vec<&str> = kept.iter().map(|t| t.cycle_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["c5"],
        "the archive must be bounded even when the eviction loop fails partway"
    );
}

#[tokio::test]
async fn task_results_do_not_appear_among_traces() {
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    memory
        .save_task_result(
            &id,
            crate::ports::TaskResult {
                task_id: "t1".into(),
                ok: true,
                output: serde_json::json!({ "done": true }),
            },
        )
        .await
        .unwrap();
    assert!(
        memory
            .recent_traces(&id, usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn traces_are_isolated_between_companies() {
    let mem = engine();
    mem.memory()
        .save_trace(
            &acme_id(),
            CompressedTrace {
                cycle_id: "c1".into(),
                summary: "acme only".into(),
                at_millis: 1,
            },
        )
        .await
        .unwrap();
    assert!(
        mem.memory()
            .recent_traces(&globex_id(), usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn re_putting_an_identical_body_keeps_the_first_stamp() {
    let mem = engine();
    let id = acme_id();
    let context = mem.context();
    let chunk = ContextChunk {
        label: "notes/one".into(),
        body: "same body".into(),
    };
    let first = context.put(&id, chunk.clone()).await.unwrap();
    let stamp = context.list(&id, "").await.unwrap()[0].stored_at_millis;
    let second = context.put(&id, chunk).await.unwrap();
    assert_eq!(first, second, "the same body mints the same address");
    let listed = context.list(&id, "").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].stored_at_millis, stamp, "the stamp did not move");
}

#[tokio::test]
async fn the_engine_id_and_fetch_modes_are_reportable() {
    let mem = engine();
    assert_eq!(mem.engine_id(), "reference");
    assert_eq!(mem.capability_names(), vec!["keyword", "vector", "hybrid"]);
}

#[tokio::test]
async fn debug_names_the_engine_and_nothing_else() {
    // The engine is process-scoped and structurally holds no company, so the
    // only thing `Debug` can say is which engine was bound — which is exactly
    // what an operator reading a boot log needs, and nothing more.
    let rendered = format!("{:?}", engine());
    assert!(rendered.contains("reference"), "{rendered}");
}

/// A rewrite replaces the record rather than accumulating versions: one live
/// item per key, so a `list` never returns a stale copy beside the new one.
#[tokio::test]
async fn upserting_a_fact_leaves_one_item_per_key() {
    let (raw, mem) = with_handle();
    let facts = mem.facts();
    let id = acme_id();
    facts.upsert(&id, &a_fact("f1", "first")).await.unwrap();
    facts.upsert(&id, &a_fact("f1", "second")).await.unwrap();
    let listed = facts.list(&id, None, None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "second");
    assert_eq!(everything(&raw).await.len(), 1);
}

// ---------------------------------------------------------------------------
// Port conformance
// ---------------------------------------------------------------------------
//
// Issue #914 requires the same conformance suite to hold for every port that
// binds an engine, so an engine-backed store is held to the identical
// assertions the fs, sqlite and mongodb backends are. This matters more here
// than for an in-tree backend: these facades encode records into an opaque
// item text and re-derive everything on the way out, so "it round-trips"
// is a property to prove against the shared suite rather than to assume.
//
// `assert_fact_store` is new coverage rather than a mirror of the cortex
// backends' three: the embedded engine implements memory + context only, so
// there has never been a cortex `FactStore` to run it against.

use crate::ports::events::EventLog;
use crate::ports::store::CompanyStore;

/// The four trait objects the suite drives: fs company and event stores, paired
/// with engine-backed memory and context. The two fs slots are the ports a
/// memory engine does not implement — same arrangement the cortex backends use.
pub(super) type ConformanceStores = (
    Arc<dyn CompanyStore>,
    Arc<dyn EventLog>,
    Arc<dyn crate::ports::MemoryStore>,
    Arc<dyn crate::ports::ContextStore>,
);
