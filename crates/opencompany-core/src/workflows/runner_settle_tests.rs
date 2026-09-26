use super::tests_capped_halt::record;
use super::tests_dry_run::GatedJournalStore;
use super::*;

/// Deps whose `DeliveryParking` journals over a caller-supplied store,
/// otherwise wired exactly like [`deps_with_parking`] — a real gate, a
/// fresh [`BlockedNodeQueue`], no continuations/gates state this test
/// needs.
fn deps_with_parking_over(
    dir: &std::path::Path,
    store: Arc<dyn crate::ports::JournalStore>,
) -> super::super::delivery::WorkflowDeliveryDeps {
    let policy = toml::from_str("mode = \"full\"\n").expect("valid [policy] block");
    let gate = Arc::new(crate::policy::ManifestApprovalGate::new(policy));
    let journal = Arc::new(crate::runtime::journal::RuntimeJournal::with_store(
        store,
        record().id,
    ));
    super::super::delivery::WorkflowDeliveryDeps {
        mail: None,
        inbox: Arc::new(crate::store::FsInboxStore::new(dir)),
        users: Arc::new(crate::store::FsOps::new(dir)),
        bootstrap_admin: None,
        channels: Vec::new(),
        notifications: None,
        parking: Some(super::super::delivery::DeliveryParking {
            approvals: gate,
            journal,
            continuations: Default::default(),
            gates: Default::default(),
            blocked_nodes: Default::default(),
            grants: Default::default(),
            events: Arc::new(crate::store::FsEventLog::new(dir)),
        }),
        events: Arc::new(crate::store::FsEventLog::new(dir)),
    }
}
/// A run that settles **two** blocked nodes in one call must arm both of
/// their in-memory stashes before either's durable mirror is awaited.
///
/// # The race this closes
///
/// `stash_blocked_agent_nodes` used to interleave the synchronous `arm()`
/// with an awaited `record_blocked_node_stashed()` call, one node at a
/// time. Both nodes' approval cards are already parked and clickable from
/// agent execution by the time this function starts — so while the first
/// node's durable write is suspended, its stash is armed but the second
/// node's is not yet, even though its card is just as clickable. A single-
/// node test cannot see this: the window only opens *between* nodes, so it
/// takes two blocked nodes in one settle, with the first node's write
/// gated open, to observe the second node's stash mid-window.
///
/// This freezes the store mid-append on the *first* matching line (the
/// first node's `BlockedNodeStashed` write) and, while still frozen, reads
/// the second node's stash straight off the queue the real resolve path
/// reads at decide time — the same `peek` a landing decision would use to
/// find what to release. Fixed: both stashes are already armed by the time
/// the first append is even attempted, so this succeeds while frozen. On
/// the old interleaved loop this fails while frozen — the second node's
/// arm has not run yet — which is exactly the failure mode: a decision
/// landing on the second node in this window finds no stash, consumes the
/// approval anyway, and the loop's own later arm then writes a stash with
/// no decision left to release it, permanently stranding the run.
///
/// Both turns are pre-armed here, matching what `park_gated_calls` does at
/// real park time (issue #1825, P1 second follow-up) — `stash_blocked_agent_nodes`
/// now skips (rather than re-arms) any turn `is_armed` reports false for,
/// since after that fix the only way a node with non-empty `approval_ids`
/// reaches this function unarmed is a released turn (see
/// `a_released_turn_is_not_resurrected_by_the_settle_pass` below), and this
/// test's whole premise depends on the settle pass actually reaching its
/// own durable-mirror loop for both nodes. Pre-arming only touches
/// `BlockedNodeQueue`, not the journal's own `blocked_stashes`, so the
/// durable append this test gates on still runs for real.
#[tokio::test]
async fn every_blocked_node_is_armed_before_the_first_journal_write_is_awaited() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(GatedJournalStore::new("BlockedNodeStashed"));
    let deps = deps_with_parking_over(dir.path(), store.clone());
    let parking = deps.parking.clone().expect("wired above");

    let blocked = vec![
        crate::ports::WorkflowBlockedNode {
            node_id: "first".to_string(),
            tools: vec!["shell".to_string()],
            approval_ids: vec!["appr-first".to_string()],
            unparkable: 0,
            stranded: 0,
            blockers: 0,
        },
        crate::ports::WorkflowBlockedNode {
            node_id: "second".to_string(),
            tools: vec!["shell".to_string()],
            approval_ids: vec!["appr-second".to_string()],
            unparkable: 0,
            stranded: 0,
            blockers: 0,
        },
    ];

    let run_id = "run-1".to_string();
    let trigger_input = serde_json::json!({ "request": "quarterly numbers" });
    let first_turn = crate::runtime::workflow_resume::workflow_node_turn_key("run-1", "first");
    let second_turn = crate::runtime::workflow_resume::workflow_node_turn_key(&run_id, "second");
    // Simulates what `park_gated_calls` already did at real park time,
    // for both nodes, before this settle pass ever runs.
    parking.blocked_nodes.arm(
        &first_turn,
        "wf-1",
        &trigger_input,
        &crate::ports::types::StartedBy::Operator,
    );
    parking.blocked_nodes.arm(
        &second_turn,
        "wf-1",
        &trigger_input,
        &crate::ports::types::StartedBy::Operator,
    );

    let handle = tokio::spawn(async move {
        stash_blocked_agent_nodes(
            Some(&deps),
            "wf-1",
            &run_id,
            &trigger_input,
            &blocked,
            &crate::ports::types::StartedBy::Operator,
        )
        .await;
    });

    // Blocks until the store is genuinely suspended inside the first
    // node's durable write — not merely about to make it.
    store.reached.notified().await;

    // While that write is still frozen: the second node's card is exactly
    // as parked and clickable as the first's, so the resolve path must
    // already be able to find its stash here.
    assert!(
        parking.blocked_nodes.peek(&second_turn).is_some(),
        "the second blocked node's stash must be armed before the first \
         node's durable journal write is even attempted, not after it \
         returns — a decision landing in this window must have something \
         to release"
    );

    store.release.notify_one();
    handle
        .await
        .expect("stash_blocked_agent_nodes does not panic");

    // Both nodes are armed once the settle finishes, and the durable
    // mirror caught up for both too.
    assert!(parking.blocked_nodes.peek(&first_turn).is_some());
    assert!(parking.blocked_nodes.peek(&second_turn).is_some());
}

/// Issue #1825 (P2, third follow-up — found by chatgpt-codex-connector): a
/// turn whose whole batch was already decided and released before this
/// settle pass runs must not be resurrected by it.
///
/// After the P1 second follow-up, every node with a non-empty
/// `approval_ids` was armed by `park_gated_calls` at real park time — so
/// if `is_armed` is false for such a node here, the only way that
/// happened is a release: an operator decided the node's *last* pending
/// card and the run dispatched (`resume_blocked_agent_node` →
/// `retire_blocked_stash`) in the window between the agent turn returning
/// and this settle pass running. Simulates that ordering directly: arms
/// the turn, releases it (as `retire_blocked_stash` would have), then
/// runs the settle pass over a `blocked` batch that still names the node
/// (the engine's own settled view predates the release). Pre-fix this
/// re-arms the released turn and durably re-stashes it, after its own
/// `BlockedNodeReleased`; post-fix the settle pass skips it and leaves no
/// trace, in memory or in the journal.
#[tokio::test]
async fn a_released_turn_is_not_resurrected_by_the_settle_pass() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn crate::ports::JournalStore> =
        Arc::new(crate::ports::journal::MemoryJournalStore::default());
    let deps = deps_with_parking_over(dir.path(), store);
    let parking = deps.parking.clone().expect("wired above");
    let journal = parking.journal.clone();

    let run_id = "run-1825-p2d".to_string();
    let trigger_input = serde_json::json!({ "request": "quarterly numbers" });
    let turn = crate::runtime::workflow_resume::workflow_node_turn_key(&run_id, "solo");

    // What `park_gated_calls` already did at real park time.
    parking.blocked_nodes.arm(
        &turn,
        "wf-1825-p2d",
        &trigger_input,
        &crate::ports::types::StartedBy::Operator,
    );
    // What deciding this turn's last card already did, in the window
    // before this settle pass got here: dispatched and retired.
    parking.blocked_nodes.release(&turn);
    journal
        .record_blocked_node_released(&turn)
        .await
        .expect("release journals cleanly");

    let blocked = vec![crate::ports::WorkflowBlockedNode {
        node_id: "solo".to_string(),
        tools: vec!["shell".to_string()],
        approval_ids: vec!["appr-solo".to_string()],
        unparkable: 0,
        stranded: 0,
        blockers: 0,
    }];
    stash_blocked_agent_nodes(
        Some(&deps),
        "wf-1825-p2d",
        &run_id,
        &trigger_input,
        &blocked,
        &crate::ports::types::StartedBy::Operator,
    )
    .await;

    assert!(
        !parking.blocked_nodes.is_armed(&turn),
        "the settle pass must not resurrect a turn that was already released before it ran"
    );
    assert!(
        journal
            .blocked_stashes()
            .into_iter()
            .all(|(t, ..)| t != turn),
        "the settle pass must not durably re-stash an already-released turn either — a \
         late approval-bank retry landing on the resurrection could make a future boot \
         dispatch this run a second time"
    );
}

/// Issue #1825 (P2, fourth follow-up — found by chatgpt-codex-connector):
/// "Make the settle-time stash check atomic with its append".
///
/// # The race this closes
///
/// `a_released_turn_is_not_resurrected_by_the_settle_pass` above proves a
/// turn released *before* the settle pass starts is not resurrected — its
/// `is_armed` check, run once while collecting `turns`, already catches
/// that. This test proves the gap that check alone does not close: a turn
/// released *during* the settle pass, while an earlier sibling's own
/// durable write is still awaited, one loop iteration before this turn's
/// own write is reached. `is_armed` was true for it when `turns` was
/// built (its card is exactly as clickable as any other), but by the time
/// its OWN await comes up, the decision has already run
/// `retire_blocked_stash` — the same interleaving
/// `every_blocked_node_is_armed_before_the_first_journal_write_is_awaited`
/// above freezes to prove the *arm* side lands in time; this freezes the
/// same point to drive a *release* through it and prove the *write* side
/// does not go ahead once one has.
///
/// Pre-fix, the durable write below runs unconditionally once a turn is in
/// `turns`, so it appends a `BlockedNodeStashed` behind the release's own
/// `BlockedNodeReleased` — durable on replay, resurrecting an
/// already-dispatched turn. Post-fix, the write is skipped and the journal
/// carries no trace of it.
#[tokio::test]
async fn a_turn_released_mid_settle_batch_is_not_stashed_behind_its_own_release() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(GatedJournalStore::new("BlockedNodeStashed"));
    let deps = deps_with_parking_over(dir.path(), store.clone());
    let parking = deps.parking.clone().expect("wired above");
    let journal = parking.journal.clone();

    let blocked = vec![
        crate::ports::WorkflowBlockedNode {
            node_id: "first".to_string(),
            tools: vec!["shell".to_string()],
            approval_ids: vec!["appr-first".to_string()],
            unparkable: 0,
            stranded: 0,
            blockers: 0,
        },
        crate::ports::WorkflowBlockedNode {
            node_id: "second".to_string(),
            tools: vec!["shell".to_string()],
            approval_ids: vec!["appr-second".to_string()],
            unparkable: 0,
            stranded: 0,
            blockers: 0,
        },
    ];

    let run_id = "run-1825-p2e".to_string();
    let trigger_input = serde_json::json!({ "request": "quarterly numbers" });
    let first_turn = crate::runtime::workflow_resume::workflow_node_turn_key(&run_id, "first");
    let second_turn = crate::runtime::workflow_resume::workflow_node_turn_key(&run_id, "second");
    // What `park_gated_calls` already did for both, at real park time,
    // before this settle pass ever runs.
    parking.blocked_nodes.arm(
        &first_turn,
        "wf-1825-p2e",
        &trigger_input,
        &crate::ports::types::StartedBy::Operator,
    );
    parking.blocked_nodes.arm(
        &second_turn,
        "wf-1825-p2e",
        &trigger_input,
        &crate::ports::types::StartedBy::Operator,
    );

    let handle = tokio::spawn(async move {
        stash_blocked_agent_nodes(
            Some(&deps),
            "wf-1825-p2e",
            &run_id,
            &trigger_input,
            &blocked,
            &crate::ports::types::StartedBy::Operator,
        )
        .await;
    });

    // Blocks until the settle pass is genuinely suspended inside the
    // FIRST node's durable write — after `turns` was built (both `first`
    // and `second` already passed their `is_armed` check), but before
    // `second`'s own write is even attempted.
    store.reached.notified().await;

    // What deciding `second`'s last card right now, mid-batch, already
    // does to the in-memory stash: `retire_blocked_stash` releases it,
    // the same call `a_released_turn_is_not_resurrected_by_the_settle_pass`
    // above reproduces directly. Its durable `BlockedNodeReleased`
    // half is deliberately NOT reproduced here while `first`'s write is
    // still frozen: `RuntimeJournal::append` takes `write_lock` around
    // the whole store call (see its doc comment), which `first`'s
    // in-flight append is still holding at this exact point, so a second
    // append attempted here would deadlock against itself — a test
    // artifact of freezing one append to observe another, not a real
    // constraint on the two decisions in production (there they run on
    // separate turns' own append calls, each taking and releasing the
    // lock in turn). The in-memory release alone is the only signal the
    // fix under test reads (`BlockedNodeQueue::is_armed`), so it is
    // sufficient on its own to pose the race.
    parking.blocked_nodes.release(&second_turn);

    // Let `first`'s write complete; the loop now reaches `second`.
    store.release.notify_one();
    handle
        .await
        .expect("stash_blocked_agent_nodes does not panic");

    assert!(
        !parking.blocked_nodes.is_armed(&second_turn),
        "a turn released mid-batch must not be resurrected in memory either"
    );
    assert!(
        journal
            .blocked_stashes()
            .into_iter()
            .all(|(t, ..)| t != second_turn),
        "a turn released mid-batch must not be durably re-stashed behind its own release — \
         a late approval-bank retry landing on the resurrection could make a future boot \
         dispatch this run a second time"
    );
    // The sibling that was never touched is unaffected.
    assert!(parking.blocked_nodes.is_armed(&first_turn));
    assert!(
        journal
            .blocked_stashes()
            .into_iter()
            .any(|(t, ..)| t == first_turn)
    );
}

// ---- merging harness transcripts into the run-output snapshot ----------

mod transcript_merge {
    use super::super::merge_transcripts;
    use serde_json::{Map, Value, json};

    fn transcripts(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(id, v)| ((*id).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn no_transcripts_leaves_the_map_identical() {
        // The overwhelmingly common case — every workflow with no agent node.
        let nodes = json!({ "cost": { "items": [] } });
        assert_eq!(merge_transcripts(&nodes, &Map::new()), nodes);
    }

    #[test]
    fn a_transcript_lands_beside_the_nodes_items() {
        let nodes = json!({ "solve": { "items": [{ "json": { "answer": 837799 } }] } });
        let merged = merge_transcripts(
            &nodes,
            &transcripts(&[(
                "solve",
                json!([{ "atMs": 0, "kind": "tool_call", "text": "shell" }]),
            )]),
        );
        // The engine's own data survives untouched...
        assert_eq!(merged["solve"]["items"][0]["json"]["answer"], 837799);
        // ...and the transcript joins it.
        assert_eq!(merged["solve"]["transcript"][0]["kind"], "tool_call");
    }

    #[test]
    fn only_the_named_nodes_are_touched() {
        let nodes = json!({
            "restate": { "items": [1] },
            "solve": { "items": [2] },
        });
        let merged =
            merge_transcripts(&nodes, &transcripts(&[("solve", json!([{ "kind": "x" }]))]));
        assert!(merged["restate"].get("transcript").is_none());
        assert!(merged["solve"].get("transcript").is_some());
    }

    #[test]
    fn a_transcript_for_an_unknown_node_is_dropped() {
        // Never invent a node the engine did not report: the snapshot is a
        // record of the run, and a node that appears only because a stray
        // transcript named it would be a lie about what executed.
        let nodes = json!({ "solve": { "items": [] } });
        let merged =
            merge_transcripts(&nodes, &transcripts(&[("ghost", json!([{ "kind": "x" }]))]));
        assert!(merged.get("ghost").is_none());
        assert_eq!(merged.as_object().unwrap().len(), 1);
    }

    #[test]
    fn a_non_object_nodes_map_passes_through() {
        // A failed drain yields `Value::Null`; coercing it into an object
        // here would fabricate a snapshot for a run that captured none.
        for nodes in [Value::Null, json!([]), json!("nope")] {
            assert_eq!(
                merge_transcripts(&nodes, &transcripts(&[("solve", json!([]))])),
                nodes
            );
        }
    }

    #[test]
    fn a_non_object_node_slot_is_left_alone() {
        let nodes = json!({ "solve": "not-an-object" });
        assert_eq!(
            merge_transcripts(&nodes, &transcripts(&[("solve", json!([]))])),
            nodes
        );
    }
}
