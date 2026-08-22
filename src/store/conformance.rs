//! Backend-agnostic port-conformance assertions.
//!
//! Each `assert_*` function drives a set of storage-port trait objects through
//! the invariants every backend must uphold, so the fs and sqlite stores prove
//! conformance against the *same* suite rather than duplicating hand-written
//! per-backend tests. The functions are parameterized over `Arc<dyn Port>` and
//! make no assumption about the concrete implementation beyond the trait
//! contract.
//!
//! Callers supply *freshly constructed, empty* stores per function: the suite
//! writes company `alpha` and company `beta` and asserts they never observe
//! each other's data, that event/ledger logs are append-only, that event
//! sequences are 0-based and strictly monotonic per company, and that
//! everything written through the ports reads back byte-identically (the
//! export-totality precondition).

use std::sync::Arc;

use crate::ports::artifacts::{ArtifactAuthor, ArtifactKind, ArtifactRecord, ArtifactStore};
use crate::ports::context::ContextStore;
use crate::ports::events::{EventLog, EventStreamItem};
use crate::ports::facts::{FactKind, FactRecord, FactStore};
use crate::ports::inbox::{EmailRecord, InboxMeta, InboxStore};
use crate::ports::login_codes::{LoginCodeRecord, LoginCodeStore};
use crate::ports::memory::MemoryStore;
use crate::ports::notifications::{Notification, NotificationStore, Subject, SubjectKind};
use crate::ports::now_millis;
use crate::ports::run_output::{
    MAX_RUN_OUTPUTS_PER_COMPANY, WorkflowRunOutputRecord, WorkflowRunOutputStore,
};
use crate::ports::sessions::{SessionKind, SessionRecord, SessionStore};
use crate::ports::skills_state::{SkillSource, SkillState, SkillStateStore};
use crate::ports::store::CompanyStore;
use crate::ports::tasks::{TaskRecord, TaskStore};
use crate::ports::types::{
    ChunkAddr, ChunkMeta, CompanyEvent, CompanyId, CompanyRecord, CompressedTrace, ContextChunk,
    EventSeq, LedgerEntry, TemplateProvenance,
};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::ports::users::{InviteRecord, UserRecord, UserRole, UserStatus, UserStore};
use crate::ports::workflow_revisions::{
    MAX_WORKFLOW_REVISIONS, WorkflowRevisionRecord, WorkflowRevisionStore,
};
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};
use futures::StreamExt;

/// A minimal valid manifest used to seed [`CompanyRecord`]s in the suite.
fn sample_manifest() -> crate::company::CompanyManifest {
    let toml_src = r#"
        [company]
        name = "Conformance Co"
        output = "widgets"

        [[agent]]
        id = "ceo"
        role = "Chief"

        [policy]
        mode = "supervised"
    "#;
    toml::from_str(toml_src).expect("parse sample manifest")
}

/// The source-template provenance the fixture seeds every record with, so the
/// export/round-trip suite proves each backend (fs, sqlite, mongodb) persists
/// and rehydrates it (issue #85).
fn sample_provenance() -> TemplateProvenance {
    TemplateProvenance {
        source_id: "conformance_template".to_string(),
        version: Some("1.2.3".to_string()),
        path: Some("companies/conformance_template".to_string()),
    }
}

/// The runtime-authored workflow graph the fixture seeds every record with, so
/// each backend (fs, sqlite, mongodb) proves it persists and rehydrates
/// console-created workflow bodies (issue #168) — on a hosted tenant this is the
/// ONLY copy of that graph.
fn sample_overlay_workflow() -> crate::ports::types::OverlayWorkflow {
    crate::ports::types::OverlayWorkflow {
        id: "conformance_flow".to_string(),
        toml: "id = \"conformance_flow\"\nname = \"Conformance flow\"\n\
               [[node]]\nid = \"start\"\nkind = \"trigger\"\nname = \"Start\"\n"
            .to_string(),
    }
}

/// The operator-set daily spend caps the fixture seeds every record with, so
/// each backend (fs, sqlite, mongodb) proves a console-set budget survives
/// persistence (issue #343).
///
/// Deliberately **three** rows, because the field's whole point is that the
/// three states stay apart across a round-trip: an ordinary cap, a legitimate
/// `0.0` cap, and an entry whose `budget_usd_daily` is `None` — "explicitly
/// uncapped", which beats a manifest cap. A backend that collapsed the last one
/// into "no row" (or into `0.0`) would silently re-impose the very cap the
/// operator cleared, and only this fixture would catch it.
fn sample_budget_overrides() -> Vec<crate::ports::types::BudgetOverride> {
    use crate::ports::types::{Actor, ActorKind, BudgetOverride};
    let admin = Actor {
        kind: ActorKind::User,
        id: "user-conformance".to_string(),
    };
    vec![
        BudgetOverride {
            agent_id: "ceo".to_string(),
            budget_usd_daily: Some(12.5),
            set_by: admin.clone(),
            at_millis: 1_700_000_000_000,
        },
        BudgetOverride {
            agent_id: "eng".to_string(),
            budget_usd_daily: Some(0.0),
            set_by: admin.clone(),
            at_millis: 1_700_000_000_001,
        },
        BudgetOverride {
            agent_id: "writer".to_string(),
            budget_usd_daily: None,
            set_by: admin,
            at_millis: 1_700_000_000_002,
        },
    ]
}

/// A populated `[policy]` override, so every store's round-trip proves a
/// console-set tier survives persistence (issue #562).
///
/// Both fields are `Some`, and `always_approve` is deliberately **not** the
/// manifest default — a round-trip that stored the manifest's own list would
/// pass whether or not the override had been persisted at all.
fn sample_policy_override() -> crate::ports::types::PolicyOverride {
    use crate::ports::types::{Actor, ActorKind, PolicyOverride};
    PolicyOverride {
        mode: Some("auto".to_string()),
        always_approve: Some(vec!["payment.send".to_string()]),
        set_by: Actor {
            kind: ActorKind::User,
            id: "user-conformance".to_string(),
        },
        at_millis: 1_700_000_000_002,
    }
}

/// The operator's edits of a manifest-declared teammate: a renamed role, a
/// cleared description (the empty-string form) and a narrowed tool scope, so a
/// backend that drops the field — or that collapses "cleared" back into "not
/// overridden" — is caught by the round-trip rather than in a console that
/// silently re-inherits the blueprint after a restart.
fn sample_agent_overrides() -> Vec<crate::ports::types::AgentOverride> {
    vec![crate::ports::types::AgentOverride {
        agent_id: "ceo".to_string(),
        name: Some("Robin".to_string()),
        role: Some("Chief Vibes".to_string()),
        description: Some(String::new()),
        tools: Some(vec!["docs.*".to_string()]),
        instructions: Some("Be exceedingly concise and decisive.".to_string()),
    }]
}

/// Builds a running record for `id` carrying a non-empty desk-order overlay (so
/// the store round-trip covers the operator desk-hierarchy field, issue #131), a
/// runtime-authored workflow body (issue #168), a populated budget-override set
/// (issue #343), a `[policy]` override (issue #562), a paused workflow id
/// (issue #276), and stamped with the sample template provenance (so round-trips
/// assert it survives persistence, issue #85).
fn record(id: &CompanyId) -> CompanyRecord {
    CompanyRecord {
        overlay_agent_edits: sample_agent_overrides(),
        // Non-empty so a backend that drops the field is caught: without the
        // tombstone the manifest is re-read on load and the removed teammate
        // comes straight back.
        overlay_retired_agents: vec!["eng".to_string()],
        id: id.clone(),
        manifest: sample_manifest(),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: vec![crate::ports::types::OverlayDeskOrder {
            desk_id: "studio".to_string(),
            ordered: vec!["ceo".to_string(), "eng".to_string()],
        }],
        overlay_desks: Vec::new(),
        overlay_workflows: vec![sample_overlay_workflow()],
        overlay_budgets: sample_budget_overrides(),
        overlay_policy: Some(sample_policy_override()),
        // Non-empty so a backend that drops the field is caught: an empty map
        // survives every possible bug, including not persisting it at all.
        overlay_desk_tools: std::collections::BTreeMap::from([(
            "studio".to_string(),
            vec!["docs.*".to_string(), "web".to_string()],
        )]),
        disabled_workflows: vec!["digest".to_string()],
        template_provenance: Some(sample_provenance()),
        setup: Some(sample_setup_answers()),
    }
}

/// The answers a first-run setup stored. Non-empty in all three fields so a
/// backend that persisted only some of them fails the round-trip below.
fn sample_setup_answers() -> crate::company::setup::SetupAnswers {
    crate::company::setup::SetupAnswers {
        industry: "E-commerce — homeware".to_string(),
        team_hint: "plus customer support".to_string(),
        automate: "Meta ads, order dispatch".to_string(),
    }
}

fn ledger_entry(i: usize) -> LedgerEntry {
    LedgerEntry {
        at_millis: now_millis(),
        kind: "inference.spend".to_string(),
        amount_usd: i as f64,
        memo: format!("entry {i}"),
    }
}

/// Every port keeps company `alpha`'s data invisible to company `beta`.
///
/// Writes across all four durable ports for `alpha` and asserts `beta` reads
/// empty from each — no key-prefix bleed, no shared table leak.
pub async fn assert_isolation_by_company(
    store: Arc<dyn CompanyStore>,
    events: Arc<dyn EventLog>,
    memory: Arc<dyn MemoryStore>,
    context: Arc<dyn ContextStore>,
) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    store.save(&record(&alpha)).await.unwrap();
    store.append_ledger(&alpha, ledger_entry(0)).await.unwrap();
    events
        .append(
            &alpha,
            CompanyEvent::OperatorMessage {
                parent: None,
                text: "a".into(),
                by: None,
                chat: None,
                deliverable: None,
            },
        )
        .await
        .unwrap();
    memory
        .save_trace(&alpha, CompressedTrace::now("c0", "s0"))
        .await
        .unwrap();
    context
        .put(
            &alpha,
            ContextChunk {
                label: "notes/intro".into(),
                body: "alpha body".into(),
            },
        )
        .await
        .unwrap();

    // `beta` was never written: every port reads empty for it.
    assert!(
        store.load(&beta).await.unwrap().is_none(),
        "beta record leaked"
    );
    assert!(
        events
            .read_from(&beta, EventSeq::new(0), usize::MAX)
            .await
            .unwrap()
            .is_empty(),
        "beta events leaked"
    );
    assert!(
        memory
            .recent_traces(&beta, usize::MAX)
            .await
            .unwrap()
            .is_empty(),
        "beta traces leaked"
    );
    assert!(
        context.list(&beta, "").await.unwrap().is_empty(),
        "beta context leaked"
    );

    // `alpha` still sees its own data.
    let loaded = store.load(&alpha).await.unwrap().expect("alpha record");
    assert_eq!(loaded.ledger.len(), 1);
    // The operator desk-order overlay survives the store round-trip (issue #131).
    assert_eq!(
        loaded.overlay_desk_order,
        vec![crate::ports::types::OverlayDeskOrder {
            desk_id: "studio".to_string(),
            ordered: vec!["ceo".to_string(), "eng".to_string()],
        }],
        "overlay_desk_order did not survive save/load"
    );
    // The runtime-authored workflow body survives the store round-trip too
    // (issue #168) — losing it would delete a hosted tenant's workflow.
    assert_eq!(
        loaded.overlay_workflows,
        vec![sample_overlay_workflow()],
        "overlay_workflows did not survive save/load"
    );
    // The operator-set daily spend caps survive the store round-trip (issue
    // #343), all three states intact — a cap, a real `0.0`, and the explicitly-
    // uncapped `None`. Collapsing the last one would silently restore the
    // manifest cap the operator had cleared.
    assert_eq!(
        loaded.overlay_budgets,
        sample_budget_overrides(),
        "overlay_budgets did not survive save/load"
    );
    assert_eq!(
        loaded.overlay_agent_edits,
        sample_agent_overrides(),
        "overlay_agent_edits did not survive save/load"
    );
    assert_eq!(
        loaded.overlay_retired_agents,
        vec!["eng".to_string()],
        "overlay_retired_agents did not survive save/load"
    );
    assert!(
        loaded
            .overlay_budgets
            .iter()
            .any(|entry| entry.agent_id == "writer" && entry.budget_usd_daily.is_none()),
        "the explicitly-uncapped override decayed into an absent or zeroed entry"
    );
    // The operator's `[policy]` override survives too (issue #562). Seeding the
    // fixture is not enough on its own: without this assertion a backend that
    // dropped the field would still pass, which is the failure the populated
    // fixture exists to catch.
    assert_eq!(
        loaded.overlay_policy,
        Some(sample_policy_override()),
        "overlay_policy did not survive save/load"
    );
    assert_eq!(
        loaded.effective_policy().mode,
        "auto",
        "the effective tier did not survive the round-trip, so a restart would \
         silently move the company back to the manifest's gate"
    );
    // Issue #276: a paused workflow survives save/load. A backend that dropped
    // this field would re-arm every schedule an operator had switched off, and
    // the only symptom would be a workflow firing again after a restart.
    assert!(
        !loaded.workflow_enabled("digest"),
        "the paused workflow id did not survive save/load"
    );
    // The per-desk tool ceilings survive too. A backend that dropped this would
    // silently widen every console-narrowed department back to the company's
    // full grant on the next restart — a capability regression whose only
    // symptom is an agent succeeding at something it had been denied.
    assert_eq!(
        loaded.effective_desk_tools("studio"),
        vec!["docs.*".to_string(), "web".to_string()],
        "overlay_desk_tools did not survive save/load"
    );
    assert_eq!(
        events
            .read_from(&alpha, EventSeq::new(0), usize::MAX)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        memory
            .recent_traces(&alpha, usize::MAX)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(context.list(&alpha, "").await.unwrap().len(), 1);
}

/// Event and ledger logs are append-only: prior entries never move or mutate
/// when new ones are written, and a record re-save does not rewrite the ledger.
pub async fn assert_append_only_event_and_ledger(
    store: Arc<dyn CompanyStore>,
    events: Arc<dyn EventLog>,
) {
    let id = CompanyId::new("alpha");
    store.save(&record(&id)).await.unwrap();

    for i in 0..3 {
        store.append_ledger(&id, ledger_entry(i)).await.unwrap();
    }
    let ledger_before = store.load(&id).await.unwrap().unwrap().ledger;
    assert_eq!(ledger_before.len(), 3);

    // Re-saving the record must not disturb the append-only ledger.
    store.save(&record(&id)).await.unwrap();
    let ledger_after = store.load(&id).await.unwrap().unwrap().ledger;
    assert_eq!(ledger_after, ledger_before, "save() rewrote the ledger");

    let s0 = events
        .append(
            &id,
            CompanyEvent::OperatorMessage {
                parent: None,
                text: "e0".into(),
                by: None,
                chat: None,
                deliverable: None,
            },
        )
        .await
        .unwrap();
    let s1 = events
        .append(
            &id,
            CompanyEvent::OperatorMessage {
                parent: None,
                text: "e1".into(),
                by: None,
                chat: None,
                deliverable: None,
            },
        )
        .await
        .unwrap();
    let prefix_before = events
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();

    // Further appends never reorder or rewrite the existing prefix.
    events
        .append(
            &id,
            CompanyEvent::OperatorMessage {
                parent: None,
                text: "e2".into(),
                by: None,
                chat: None,
                deliverable: None,
            },
        )
        .await
        .unwrap();
    let all = events
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&all[..2], &prefix_before[..], "append reordered the prefix");
    assert_eq!(all[0].seq, s0);
    assert_eq!(all[1].seq, s1);
    assert_eq!(all.len(), 3);
    // More ledger appends still grow monotonically after the re-save.
    store.append_ledger(&id, ledger_entry(99)).await.unwrap();
    let grown = store.load(&id).await.unwrap().unwrap().ledger;
    assert_eq!(grown.len(), 4);
    assert_eq!(grown[..3], ledger_before[..]);
}

/// Event sequences are 0-based, increase by exactly one per append, and are
/// independent per company.
pub async fn assert_monotonic_event_seq(events: Arc<dyn EventLog>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    for expected in 0..5u64 {
        let seq = events
            .append(
                &alpha,
                CompanyEvent::OperatorMessage {
                    parent: None,
                    text: format!("a{expected}"),
                    by: None,
                    chat: None,
                    deliverable: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(seq, EventSeq::new(expected), "alpha seq not 0-based +1");
    }

    // A second company starts its own sequence at 0.
    let first_beta = events
        .append(
            &beta,
            CompanyEvent::OperatorMessage {
                parent: None,
                text: "b0".into(),
                by: None,
                chat: None,
                deliverable: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        first_beta,
        EventSeq::new(0),
        "beta seq did not restart at 0"
    );

    // Stored seqs read back in order and match the returned values.
    let stored = events
        .read_from(&alpha, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    for (i, ev) in stored.iter().enumerate() {
        assert_eq!(ev.seq, EventSeq::new(i as u64));
        assert_eq!(ev.company, alpha);
    }
    // `read_from` honours the `seq >=` lower bound.
    let tail = events
        .read_from(&alpha, EventSeq::new(3), usize::MAX)
        .await
        .unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].seq, EventSeq::new(3));
}

/// A live receiver that falls behind the 256-slot backend broadcast ring must
/// report loss before delivering its retained tail. Silent `Lagged` handling
/// leaves a console unable to distinguish a quiet company from a stale one.
pub async fn assert_event_subscription_surfaces_gap(events: Arc<dyn EventLog>) {
    let id = CompanyId::new("gap");
    let mut stream = events.subscribe(&id);
    for seq in 0..300 {
        events
            .append(
                &id,
                CompanyEvent::OperatorMessage {
                    parent: None,
                    text: format!("event {seq}"),
                    by: None,
                    chat: None,
                    deliverable: None,
                },
            )
            .await
            .unwrap();
    }

    assert_eq!(
        stream.next().await,
        Some(EventStreamItem::Gap { missed: 44 }),
        "a lagged receiver must announce exactly the entries its 256-slot buffer lost"
    );
    let Some(EventStreamItem::Event(first_retained)) = stream.next().await else {
        panic!("the retained tail must continue after its gap signal");
    };
    assert_eq!(first_retained.seq, EventSeq::new(44));
}

/// `read_before` returns a bounded, newest-first page before an exclusive
/// cursor. This is the primitive transcript pagination uses, so every durable
/// EventLog backend must exercise its production query/stream implementation.
pub async fn assert_event_read_before(events: Arc<dyn EventLog>) {
    let id = CompanyId::new("history-page");
    let mut seqs = Vec::new();
    for text in ["zero", "one", "two", "three"] {
        seqs.push(
            events
                .append(
                    &id,
                    CompanyEvent::OperatorMessage {
                        parent: None,
                        text: text.to_string(),
                        by: None,
                        chat: None,
                        deliverable: None,
                    },
                )
                .await
                .unwrap(),
        );
    }

    let tail = events.read_before(&id, None, 2).await.unwrap();
    assert_eq!(
        tail.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![seqs[3], seqs[2]],
        "the tail page must be newest-first"
    );

    let before = events.read_before(&id, Some(seqs[3]), 2).await.unwrap();
    assert_eq!(
        before.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![seqs[2], seqs[1]],
        "the cursor is exclusive and the limit is enforced"
    );

    assert!(
        events
            .read_before(&id, Some(seqs[0]), 2)
            .await
            .unwrap()
            .is_empty(),
        "nothing precedes the first event"
    );
    assert!(
        events.read_before(&id, None, 0).await.unwrap().is_empty(),
        "a zero limit never reads a page"
    );
}

/// Asserts the [`EventLog`] retention contract (issue #275): the default
/// policy is inert, permanent kinds survive any policy, sequences are never
/// renumbered or reused, and a pruned log still reads and appends correctly.
///
/// Deliberately says nothing about the age bound. `append` stamps `at_millis`
/// from the wall clock, so a backend test cannot place an event in the past
/// without a clock seam none of the three backends has. Age selection is a
/// property of [`plan_prune`](crate::ports::events::plan_prune), which is pure
/// and unit-tested against synthetic timestamps in `ports::events`; what a
/// *backend* has to prove is that it removes exactly the entries that function
/// names, which is what this covers.
pub async fn assert_event_retention(events: Arc<dyn EventLog>) {
    use crate::ports::events::RetentionPolicy;

    let acme = CompanyId::new("acme");

    let run_started = |n: u64| CompanyEvent::WorkflowRunStarted {
        workflow_id: "wf".to_string(),
        run_id: format!("run-{n}"),
        scheduled: false,
    };
    let audit = |n: u64| CompanyEvent::LifecycleChanged {
        from: "running".to_string(),
        to: format!("paused-{n}"),
        by: crate::ports::types::Actor {
            kind: crate::ports::types::ActorKind::Operator,
            id: "operator".to_string(),
        },
    };

    // Interleave prunable run outcomes with permanent audit entries so the
    // pass has to discriminate rather than truncate a prefix.
    for n in 0..5u64 {
        events.append(&acme, run_started(n)).await.unwrap();
        events.append(&acme, audit(n)).await.unwrap();
    }
    let before = events
        .read_from(&acme, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert_eq!(before.len(), 10, "fixture seeded 10 events");

    // 1. The default policy is a no-op. Retention is opt-in.
    let report = events
        .prune(&acme, &RetentionPolicy::default())
        .await
        .unwrap();
    assert_eq!(report.removed, 0, "default policy removed something");
    assert_eq!(report.scanned, 10);
    let after_noop = events
        .read_from(&acme, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert_eq!(after_noop, before, "no-op prune changed the log");

    // 2. A count bound removes only prunable kinds, keeping the newest.
    let report = events
        .prune(&acme, &RetentionPolicy::with_max_entries_per_kind(2))
        .await
        .unwrap();
    // 5 run-starts at seqs 0,2,4,6,8; the bound keeps the newest 2 (seqs 8, 6)
    // and discards the other 3. The permanent audit rows are never candidates,
    // and the seq watermark (9) is an audit row anyway.
    assert_eq!(
        report.removed, 3,
        "count bound should discard the 3 oldest of 5 run-starts"
    );

    let kept = events
        .read_from(&acme, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    let lifecycles = kept
        .iter()
        .filter(|e| e.event.kind() == "LifecycleChanged")
        .count();
    assert_eq!(lifecycles, 5, "a permanent kind was pruned");
    let runs: Vec<String> = kept
        .iter()
        .filter_map(|e| match &e.event {
            CompanyEvent::WorkflowRunStarted { run_id, .. } => Some(run_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        runs,
        vec!["run-3".to_string(), "run-4".to_string()],
        "count bound did not keep the newest run outcomes"
    );

    // 3. Sequences are not renumbered: every surviving entry keeps the number
    //    it was assigned, so a stored cross-reference still resolves.
    for ev in &kept {
        let original = before
            .iter()
            .find(|b| b.seq == ev.seq)
            .expect("surviving seq was never issued");
        assert_eq!(&original.event, &ev.event, "seq {:?} renumbered", ev.seq);
    }
    assert_eq!(
        report.oldest_retained,
        kept.first().map(|e| e.seq),
        "report disagrees with the log about the oldest survivor"
    );

    // 4. `read_from` tolerates the gaps a prune leaves: a cursor parked on a
    //    removed sequence resumes at the next survivor rather than erroring.
    let removed_seq = before
        .iter()
        .map(|e| e.seq)
        .find(|seq| !kept.iter().any(|k| k.seq == *seq))
        .expect("something was removed");
    let resumed = events
        .read_from(&acme, removed_seq, usize::MAX)
        .await
        .unwrap();
    assert!(
        resumed.first().is_some_and(|e| e.seq > removed_seq),
        "read_from did not resume past a pruned sequence"
    );

    // 5. The next append must not reuse a sequence a survivor still holds.
    //    This is the invariant that makes pruning safe for the fs and sqlite
    //    backends, which derive the next sequence from the highest present.
    let highest_before = kept.iter().map(|e| e.seq).max().unwrap();
    let next = events.append(&acme, audit(99)).await.unwrap();
    assert!(
        next > highest_before,
        "append reused sequence {next:?} after a prune (highest surviving was {highest_before:?})"
    );
    let reread = events
        .read_from(&acme, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    let mut seqs: Vec<EventSeq> = reread.iter().map(|e| e.seq).collect();
    let unique = seqs.len();
    seqs.dedup();
    assert_eq!(unique, seqs.len(), "duplicate sequences after prune+append");

    // 6. Issue #983: a turn's accept bracket is prunable, its failure bracket is
    //    not, and pruning the accept must not break a live turn's read-back.
    //
    //    The pairing is the point. `TurnStarted` is one frame per operator
    //    message on a busy desk and its meaning is spent once the turn settles;
    //    `TurnFailed` is the only record that a question was accepted and never
    //    answered, so losing it would silently un-report a lost turn. And the
    //    two are joined by `turn_id`, not by sequence — which is what makes the
    //    accept safe to discard: nothing points *at* it, so a turn still reads
    //    back through its row and its failure line after its start is gone.
    let bravo = CompanyId::new("bravo");
    for n in 0..4u64 {
        events
            .append(
                &bravo,
                CompanyEvent::TurnStarted {
                    turn_id: format!("turn-{n}"),
                    chat_id: "general".to_string(),
                    parent: None,
                    by: None,
                },
            )
            .await
            .unwrap();
    }
    events
        .append(
            &bravo,
            CompanyEvent::TurnFailed {
                turn_id: "turn-0".to_string(),
                error: "the host restarted".to_string(),
            },
        )
        .await
        .unwrap();

    let report = events
        .prune(&bravo, &RetentionPolicy::with_max_entries_per_kind(1))
        .await
        .unwrap();
    assert_eq!(
        report.removed, 3,
        "TurnStarted must be prunable, keeping only the newest"
    );

    let kept = events
        .read_from(&bravo, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    let starts: Vec<&str> = kept
        .iter()
        .filter_map(|e| match &e.event {
            CompanyEvent::TurnStarted { turn_id, .. } => Some(turn_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(starts, ["turn-3"], "the newest accept must survive");
    let settles: Vec<&str> = kept
        .iter()
        .filter_map(|e| match &e.event {
            CompanyEvent::TurnFailed { turn_id, .. } => Some(turn_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        settles,
        ["turn-0"],
        "a turn's failure is the record that it was never answered; it is permanent"
    );
}

/// Everything written through the ports reads back through the ports,
/// byte-identically — the totality precondition an export relies on.
pub async fn assert_export_totality(
    store: Arc<dyn CompanyStore>,
    events: Arc<dyn EventLog>,
    memory: Arc<dyn MemoryStore>,
    context: Arc<dyn ContextStore>,
) {
    let id = CompanyId::new("alpha");
    store.save(&record(&id)).await.unwrap();

    let mut ledger = Vec::new();
    for i in 0..4 {
        let e = ledger_entry(i);
        ledger.push(e.clone());
        store.append_ledger(&id, e).await.unwrap();
    }

    let mut appended = Vec::new();
    for i in 0..4 {
        let ev = CompanyEvent::OperatorMessage {
            parent: None,
            text: format!("event {i}"),
            by: None,
            chat: None,
            deliverable: None,
        };
        events.append(&id, ev.clone()).await.unwrap();
        appended.push(ev);
    }

    let mut traces = Vec::new();
    for i in 0..3 {
        let t = CompressedTrace::now(format!("c{i}"), format!("summary {i}"));
        traces.push(t.clone());
        memory.save_trace(&id, t).await.unwrap();
    }

    let bodies = ["export alpha", "export beta", "export gamma"];
    let mut addrs = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        let addr = context
            .put(
                &id,
                ContextChunk {
                    label: format!("doc/{i}"),
                    body: (*body).to_string(),
                },
            )
            .await
            .unwrap();
        addrs.push(addr);
    }

    // Company record + ledger round-trip.
    let loaded = store.load(&id).await.unwrap().expect("record");
    assert_eq!(loaded.manifest.company.name, "Conformance Co");
    assert_eq!(loaded.lifecycle, "running");
    assert_eq!(loaded.ledger, ledger);
    // Issue #85: the source-template provenance persists through the port and
    // rehydrates intact — asserted here for every backend the suite runs against
    // (fs, sqlite, mongodb), which is the cross-backend durability guarantee.
    assert_eq!(
        loaded.template_provenance,
        Some(sample_provenance()),
        "template provenance did not round-trip through the store"
    );
    // First-run setup's answers persist for every backend too. Phase 2 builds
    // this company's workflows from them, so a backend that dropped them would
    // make the operator describe their business a second time.
    assert_eq!(
        loaded.setup,
        Some(sample_setup_answers()),
        "setup answers did not round-trip through the store"
    );
    // Issue #168: the runtime-authored graph bodies round-trip too — an export
    // that dropped them would lose every console-created workflow.
    assert_eq!(
        loaded.overlay_workflows,
        vec![sample_overlay_workflow()],
        "overlay_workflows did not round-trip through the store"
    );
    // Issue #343: the console-set daily caps round-trip on every backend. This
    // is what makes "no redeploy" durable rather than in-memory — a cap raised
    // from the Team page has to still be raised after the process restarts.
    assert_eq!(
        loaded.overlay_budgets,
        sample_budget_overrides(),
        "overlay_budgets did not round-trip through the store"
    );
    // The console-shaped roster round-trips on every backend, for the same
    // reason: a teammate renamed from the Team page has to still be renamed
    // after the process restarts, or the edit was never really made.
    assert_eq!(
        loaded.overlay_agent_edits,
        sample_agent_overrides(),
        "overlay_agent_edits did not round-trip through the store"
    );
    assert_eq!(
        loaded.overlay_retired_agents,
        vec!["eng".to_string()],
        "overlay_retired_agents did not round-trip through the store — a removed \
         teammate would come back on the next load"
    );
    // Issue #562: the console-set tier round-trips on every backend, for the
    // same reason — an approval gate that forgets across a restart is not a gate.
    assert_eq!(
        loaded.overlay_policy,
        Some(sample_policy_override()),
        "overlay_policy did not round-trip through the store"
    );
    assert_eq!(loaded.effective_policy().mode, "auto");
    // Issue #276: the pause switch round-trips on every backend, for the same
    // reason the caps do — a switch that forgets across a restart is not a
    // safety switch.
    assert!(
        !loaded.workflow_enabled("digest"),
        "the paused workflow id did not round-trip through the store"
    );

    // Full event log round-trips with seqs and payloads intact.
    let read = events
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert_eq!(read.len(), appended.len());
    for (i, stored) in read.iter().enumerate() {
        assert_eq!(stored.seq, EventSeq::new(i as u64));
        assert_eq!(stored.event, appended[i]);
    }

    // All traces round-trip, newest last.
    let recent = memory.recent_traces(&id, usize::MAX).await.unwrap();
    assert_eq!(recent, traces);

    // Every context chunk is listable and its body reads back exactly.
    let metas = context.list(&id, "").await.unwrap();
    assert_eq!(metas.len(), bodies.len());
    for (addr, body) in addrs.iter().zip(bodies.iter()) {
        let read_body = context.peek(&id, addr, None).await.unwrap();
        assert_eq!(&read_body, body);
    }
    // Search finds a written body.
    let hits = context.search(&id, "gamma", usize::MAX).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("gamma"));

    // Delete removes the addressed chunk — gone from the list, gone from peek —
    // reports true for the removal and false for a second attempt, and leaves
    // every other chunk untouched. `false` on the re-delete is the contract a
    // forget tool leans on: forgetting the already-forgotten is a no-op, never
    // a fault.
    let victim = addrs[0].clone();
    assert!(context.delete(&id, &victim).await.unwrap());
    assert!(!context.delete(&id, &victim).await.unwrap());
    let metas = context.list(&id, "").await.unwrap();
    assert_eq!(metas.len(), bodies.len() - 1);
    assert!(
        metas.iter().all(|m| m.addr != victim),
        "deleted addr still listed"
    );
    assert!(
        context.peek(&id, &victim, None).await.is_err(),
        "deleted chunk still peekable"
    );
    for addr in &addrs[1..] {
        context.peek(&id, addr, None).await.unwrap();
    }
}

/// Asserts the [`InboxStore`] contract: per-company isolation, per-inbox
/// filtering, append order, pagination, metadata, and read-marking.
pub async fn assert_inbox_store(inbox: Arc<dyn InboxStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    let email = |id: &str, mailbox: &str, outbound: bool, at: u64| EmailRecord {
        id: id.to_string(),
        inbox: mailbox.to_string(),
        from_name: "Sender".to_string(),
        from_email: "sender@example.com".to_string(),
        subject: format!("subject {id}"),
        body: format!("body {id}"),
        at_millis: at,
        read: false,
        outbound,
    };

    // alpha has two messages in `ceo` and one outbound in `sales`.
    inbox
        .append(&alpha, &email("a1", "ceo", false, 1))
        .await
        .unwrap();
    inbox
        .append(&alpha, &email("a2", "sales", true, 2))
        .await
        .unwrap();
    inbox
        .append(&alpha, &email("a3", "ceo", true, 3))
        .await
        .unwrap();
    // beta has an unrelated message; it must never leak into alpha.
    inbox
        .append(&beta, &email("b1", "ceo", false, 4))
        .await
        .unwrap();

    // Per-inbox listing filters and preserves append order.
    let ceo = inbox.messages(&alpha, "ceo", usize::MAX, 0).await.unwrap();
    assert_eq!(ceo.len(), 2);
    assert_eq!(ceo[0].id, "a1");
    assert_eq!(ceo[1].id, "a3");
    assert!(ceo[1].outbound);

    // Pagination: offset + limit slice the thread.
    let page = inbox.messages(&alpha, "ceo", 1, 1).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, "a3");

    // Isolation: alpha's `ceo` and beta's `ceo` are distinct.
    let beta_ceo = inbox.messages(&beta, "ceo", usize::MAX, 0).await.unwrap();
    assert_eq!(beta_ceo.len(), 1);
    assert_eq!(beta_ceo[0].id, "b1");

    // Enumeration lists exactly the inboxes with mail (default enabled meta).
    let mut names: Vec<String> = inbox
        .inboxes(&alpha)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.key)
        .collect();
    names.sort();
    assert_eq!(names, vec!["ceo".to_string(), "sales".to_string()]);

    // Explicit metadata overrides the synthesized default and adds empty inboxes.
    inbox
        .set_enabled(
            &alpha,
            "support",
            &InboxMeta {
                key: "support".to_string(),
                name: "Support".to_string(),
                address: "support@acme.test".to_string(),
                enabled: true,
            },
        )
        .await
        .unwrap();
    let support = inbox
        .inboxes(&alpha)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.key == "support")
        .expect("support meta present");
    assert_eq!(support.address, "support@acme.test");
    assert!(support.enabled);

    // mark_read marks the named ids and reports remaining unread.
    let remaining = inbox
        .mark_read(&alpha, "ceo", Some(&["a1".to_string()]))
        .await
        .unwrap();
    assert_eq!(remaining, 1, "a3 remains unread");
    let ceo = inbox.messages(&alpha, "ceo", usize::MAX, 0).await.unwrap();
    assert!(ceo.iter().find(|m| m.id == "a1").unwrap().read);
    assert!(!ceo.iter().find(|m| m.id == "a3").unwrap().read);

    // mark_read with None marks the whole inbox read.
    let remaining = inbox.mark_read(&alpha, "ceo", None).await.unwrap();
    assert_eq!(remaining, 0);

    // An empty inbox reads back empty.
    assert!(
        inbox
            .messages(&alpha, "unknown", usize::MAX, 0)
            .await
            .unwrap()
            .is_empty()
    );

    // --- has_inbound_from: the established-correspondent gate ---------------
    //
    // Callers use this as a SECURITY gate (a workflow may only email an address
    // that has written in first), so the contract is asserted here rather than
    // left to whichever backend happens to be wired. A backend that overrides
    // the default with an indexed lookup must still satisfy every case below.
    let from = |id: &str, mailbox: &str, sender: &str, outbound: bool| EmailRecord {
        from_email: sender.to_string(),
        ..email(id, mailbox, outbound, 10)
    };
    inbox
        .append(&alpha, &from("c1", "ops", "ada@example.com", false))
        .await
        .unwrap();
    // `grace` only ever RECEIVED mail from this company — never wrote in.
    inbox
        .append(&alpha, &from("c2", "ops", "grace@example.com", true))
        .await
        .unwrap();

    assert!(
        inbox
            .has_inbound_from(&alpha, "ops", "ada@example.com")
            .await
            .unwrap(),
        "an address that wrote in is an established correspondent"
    );
    assert!(
        !inbox
            .has_inbound_from(&alpha, "ops", "grace@example.com")
            .await
            .unwrap(),
        "OUTBOUND mail must not establish a correspondent — otherwise one send \
         would authorize the next"
    );
    assert!(
        !inbox
            .has_inbound_from(&alpha, "ops", "stranger@example.com")
            .await
            .unwrap()
    );
    // Case and surrounding whitespace do not change who someone is.
    assert!(
        inbox
            .has_inbound_from(&alpha, "ops", "  Ada@EXAMPLE.com ")
            .await
            .unwrap()
    );
    // A blank needle matches nobody, rather than matching anybody.
    assert!(!inbox.has_inbound_from(&alpha, "ops", "   ").await.unwrap());
    // Wrong mailbox, and wrong company, both miss.
    assert!(
        !inbox
            .has_inbound_from(&alpha, "ceo", "ada@example.com")
            .await
            .unwrap()
    );
    assert!(
        !inbox
            .has_inbound_from(&beta, "ops", "ada@example.com")
            .await
            .unwrap()
    );
}

/// Asserts the [`TaskStore`] contract: per-company isolation, upsert semantics,
/// and delete.
pub async fn assert_task_store(tasks: Arc<dyn TaskStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let task = |id: &str, col: &str, at: u64| TaskRecord {
        id: id.to_string(),
        title: format!("title {id}"),
        note: Some(format!("note {id}")),
        column: col.to_string(),
        priority: "medium".to_string(),
        assignee: "Strategy desk".to_string(),
        updated_at_millis: at,
        origin_chat_id: None,
        parent_task_id: None,
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
    };

    tasks.upsert(&alpha, &task("t1", "todo", 1)).await.unwrap();
    tasks.upsert(&alpha, &task("t2", "todo", 2)).await.unwrap();
    tasks.upsert(&beta, &task("b1", "done", 3)).await.unwrap();

    // Isolation + newest-first ordering.
    let list = tasks.list(&alpha).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, "t2");
    assert!(
        tasks
            .list(&beta)
            .await
            .unwrap()
            .iter()
            .all(|t| t.id == "b1")
    );

    // Upsert replaces in place (a drag moves a card's column).
    tasks.upsert(&alpha, &task("t1", "done", 5)).await.unwrap();
    let list = tasks.list(&alpha).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list.iter().find(|t| t.id == "t1").unwrap().column, "done");

    // Delete.
    assert!(tasks.delete(&alpha, "t1").await.unwrap());
    assert!(!tasks.delete(&alpha, "t1").await.unwrap());
    assert_eq!(tasks.list(&alpha).await.unwrap().len(), 1);

    // Issue #337: a card carrying a full plan round-trips **byte-identically**
    // on every backend.
    //
    // Worth its own leg rather than folding a plan into the fixture above,
    // because the failure it guards is quiet and backend-specific: the fs
    // bundle stores the board as a JSON array while sqlite and mongodb store
    // each card as a `task_json` string, so a nested structure that survives
    // one can be flattened, reordered or dropped by another — and a plan whose
    // `prerequisites` came back empty would read as "this card needs nothing",
    // which is the one wrong answer that lets a card dispatch into work it
    // cannot do. Comparing the whole record rather than spot-checking fields is
    // what makes a silently-dropped field fail here.
    let planned = TaskRecord {
        plan: Some(crate::ports::tasks::TaskPlan {
            description: "Publish the release notes".to_string(),
            steps: vec![
                crate::ports::tasks::PlanStep {
                    title: "Draft".to_string(),
                    detail: "against the tagged version".to_string(),
                    estimated_cost_usd: Some(0.25),
                    estimated_minutes: Some(30),
                },
                // A step with no estimates at all — the skip-if-none half.
                crate::ports::tasks::PlanStep {
                    title: "Publish".to_string(),
                    detail: "once review signs off".to_string(),
                    estimated_cost_usd: None,
                    estimated_minutes: None,
                },
            ],
            // One of every verdict, so a backend that mangled the enum
            // encoding fails rather than happening to round-trip the one
            // value the fixture used.
            prerequisites: vec![
                crate::ports::tasks::Prerequisite {
                    kind: crate::ports::tasks::PrereqKind::Connection,
                    name: "github".to_string(),
                    status: crate::ports::tasks::PrereqStatus::Satisfied,
                    note: "github is connected".to_string(),
                },
                crate::ports::tasks::Prerequisite {
                    kind: crate::ports::tasks::PrereqKind::Mcp,
                    name: "search".to_string(),
                    status: crate::ports::tasks::PrereqStatus::Missing,
                    note: "no MCP server called `search` is configured".to_string(),
                },
                crate::ports::tasks::Prerequisite {
                    kind: crate::ports::tasks::PrereqKind::Permission,
                    name: "web".to_string(),
                    status: crate::ports::tasks::PrereqStatus::NeedsApproval,
                    note: "policy stops it for a person".to_string(),
                },
                crate::ports::tasks::Prerequisite {
                    kind: crate::ports::tasks::PrereqKind::Other,
                    name: "something odd".to_string(),
                    status: crate::ports::tasks::PrereqStatus::Unknown,
                    note: "not checked".to_string(),
                },
            ],
            risks: vec!["the tag may not exist yet".to_string()],
            verification: "the notes are live".to_string(),
            scope: "the notes only".to_string(),
            proposed_assignee: Some("maya".to_string()),
            // Issue #1106. Populated here even though a real plan never carries
            // both this and `proposed_assignee` — this fixture's job is to make
            // a silently-dropped field fail, and a field left empty would
            // round-trip through a backend that drops it entirely.
            assignee_candidates: vec![
                crate::ports::tasks::AssigneeCandidate {
                    id: "maya".to_string(),
                    reason: "writes the release notes today".to_string(),
                },
                crate::ports::tasks::AssigneeCandidate {
                    id: "devrel".to_string(),
                    reason: "owns everything that ships to developers".to_string(),
                },
            ],
            planned_at_millis: 1_234,
        }),
        ..task("t-planned", "planning", 9)
    };
    tasks.upsert(&alpha, &planned).await.unwrap();
    let read_back = tasks
        .list(&alpha)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == "t-planned")
        .expect("the planned card persists");
    assert_eq!(
        read_back, planned,
        "a plan must survive the round trip whole"
    );

    // And a card with no plan reads back with none — the additive-wire
    // contract, checked on the backend rather than only on the serde shape.
    assert!(
        tasks
            .list(&alpha)
            .await
            .unwrap()
            .iter()
            .find(|t| t.id == "t2")
            .expect("t2")
            .plan
            .is_none()
    );
}

/// Asserts the [`UserStore`] contract: per-company isolation, email uniqueness,
/// exact (non-normalizing) email lookup, and invite handling.
pub async fn assert_user_store(users: Arc<dyn UserStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let user = |id: &str, email: &str, at: u64| UserRecord {
        id: id.to_string(),
        email: email.to_string(),
        display_name: Some(format!("name {id}")),
        role: UserRole::Member,
        status: UserStatus::Active,
        password_hash: None,
        must_change_password: false,
        created_at_millis: at,
        last_seen_at_millis: None,
        updated_at_millis: at,
    };

    users
        .upsert_user(&alpha, &user("u1", "ada@example.com", 1))
        .await
        .unwrap();
    users
        .upsert_user(&alpha, &user("u2", "bob@example.com", 2))
        .await
        .unwrap();
    users
        .upsert_user(&beta, &user("b1", "eve@example.com", 3))
        .await
        .unwrap();

    // Isolation + newest-first ordering.
    let list = users.list_users(&alpha).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, "u2");
    assert_eq!(users.list_users(&beta).await.unwrap().len(), 1);

    // A user of one company is invisible to another, by id and by email.
    assert!(users.get_user(&beta, "u1").await.unwrap().is_none());
    assert!(
        users
            .find_user_by_email(&beta, "ada@example.com")
            .await
            .unwrap()
            .is_none()
    );

    // Email lookup finds the right user.
    let found = users
        .find_user_by_email(&alpha, "ada@example.com")
        .await
        .unwrap()
        .expect("ada is a user of alpha");
    assert_eq!(found.id, "u1");

    // Lookup is exact: stores never normalize on the caller's behalf, so a
    // caller that forgets `normalize_email` misses rather than silently
    // matching an address it did not ask for.
    assert!(
        users
            .find_user_by_email(&alpha, "Ada@Example.com")
            .await
            .unwrap()
            .is_none()
    );

    // Upsert replaces in place by id.
    let mut promoted = user("u1", "ada@example.com", 1);
    promoted.role = UserRole::Admin;
    users.upsert_user(&alpha, &promoted).await.unwrap();
    assert_eq!(users.list_users(&alpha).await.unwrap().len(), 2);
    assert_eq!(
        users.get_user(&alpha, "u1").await.unwrap().unwrap().role,
        UserRole::Admin
    );

    // Email is unique within a company: a different id may not take a taken
    // address.
    let clash = users
        .upsert_user(&alpha, &user("u3", "ada@example.com", 4))
        .await;
    assert!(
        clash.is_err(),
        "a second user must not be able to claim ada@example.com"
    );

    // ...but the same address in another company is a different person.
    users
        .upsert_user(&beta, &user("b2", "ada@example.com", 5))
        .await
        .expect("alpha's ada must not block beta's ada");

    // Delete.
    assert!(users.delete_user(&alpha, "u1").await.unwrap());
    assert!(!users.delete_user(&alpha, "u1").await.unwrap());
    assert_eq!(users.list_users(&alpha).await.unwrap().len(), 1);

    // --- invites ---
    let invite = |id: &str, email: &str, at: u64| InviteRecord {
        id: id.to_string(),
        email: email.to_string(),
        role: UserRole::Member,
        invited_by: "operator".to_string(),
        created_at_millis: at,
        expires_at_millis: at + 1_000,
        accepted_at_millis: None,
        notified_at_millis: None,
    };

    users
        .upsert_invite(&alpha, &invite("i1", "carol@example.com", 1))
        .await
        .unwrap();
    users
        .upsert_invite(&beta, &invite("i2", "dave@example.com", 2))
        .await
        .unwrap();

    // Invites are per-company too.
    assert_eq!(users.list_invites(&alpha).await.unwrap().len(), 1);
    assert!(
        users
            .find_invite_by_email(&beta, "carol@example.com")
            .await
            .unwrap()
            .is_none(),
        "an invite to alpha must not admit anyone to beta"
    );
    assert_eq!(
        users
            .find_invite_by_email(&alpha, "carol@example.com")
            .await
            .unwrap()
            .unwrap()
            .id,
        "i1"
    );

    // One outstanding invite per address.
    assert!(
        users
            .upsert_invite(&alpha, &invite("i9", "carol@example.com", 3))
            .await
            .is_err()
    );

    // Marking an invite redeemed is an in-place upsert.
    let mut accepted = invite("i1", "carol@example.com", 1);
    accepted.accepted_at_millis = Some(9);
    users.upsert_invite(&alpha, &accepted).await.unwrap();
    assert_eq!(
        users
            .find_invite_by_email(&alpha, "carol@example.com")
            .await
            .unwrap()
            .unwrap()
            .accepted_at_millis,
        Some(9)
    );

    // The mailed stamp round-trips through the backend, both ways (issue
    // #584). It is what the console reads to say "invite email sent", so a
    // backend that dropped it would render every invite as un-mailed.
    let mut notified = invite("i1", "carol@example.com", 1);
    notified.notified_at_millis = Some(11);
    users.upsert_invite(&alpha, &notified).await.unwrap();
    assert_eq!(
        users
            .find_invite_by_email(&alpha, "carol@example.com")
            .await
            .unwrap()
            .unwrap()
            .notified_at_millis,
        Some(11),
        "a stored notified_at_millis must survive the round trip"
    );
    // And back to unset: a store that only ever wrote the field when present
    // would leave a stale timestamp behind on a re-invite.
    users
        .upsert_invite(&alpha, &invite("i1", "carol@example.com", 1))
        .await
        .unwrap();
    assert_eq!(
        users
            .find_invite_by_email(&alpha, "carol@example.com")
            .await
            .unwrap()
            .unwrap()
            .notified_at_millis,
        None,
        "clearing notified_at_millis must persist as cleared"
    );

    // Stamping is an update, never an upsert. Mail delivery is a network round
    // trip, and the record the route holds across it goes stale the moment an
    // admin revokes the invite — so a backend that implemented the stamp as a
    // full-record write would restore an address the admin had just removed
    // from the allowlist.
    assert!(
        users.mark_invite_notified(&alpha, "i1", 12).await.unwrap(),
        "stamping an outstanding invite must report that it landed"
    );
    let stamped = users
        .find_invite_by_email(&alpha, "carol@example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stamped.notified_at_millis, Some(12));
    assert_eq!(
        stamped.email, "carol@example.com",
        "stamping must leave every other field alone"
    );
    assert_eq!(stamped.created_at_millis, 1);

    assert!(users.delete_invite(&alpha, "i1").await.unwrap());
    assert!(!users.delete_invite(&alpha, "i1").await.unwrap());
    assert!(
        !users.mark_invite_notified(&alpha, "i1", 13).await.unwrap(),
        "stamping a revoked invite must report that nothing was updated"
    );
    assert!(
        users.list_invites(&alpha).await.unwrap().is_empty(),
        "stamping must never recreate a revoked invite"
    );
}

/// Asserts the [`SessionStore`] contract: per-company isolation, token-hash
/// lookup, revocation, and expiry purging.
pub async fn assert_session_store(sessions: Arc<dyn SessionStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let session = |id: &str, hash: &str, user: &str, expires: u64| SessionRecord {
        id: id.to_string(),
        token_hash: hash.to_string(),
        user_id: user.to_string(),
        created_at_millis: 1,
        expires_at_millis: expires,
        user_agent: None,
        kind: SessionKind::Browser,
        label: None,
    };

    sessions
        .create(&alpha, &session("s1", "hash-1", "u1", 100))
        .await
        .unwrap();
    sessions
        .create(&alpha, &session("s2", "hash-2", "u1", 100))
        .await
        .unwrap();
    sessions
        .create(&alpha, &session("s3", "hash-3", "u2", 100))
        .await
        .unwrap();

    // Lookup is by token hash — the only session read path.
    assert_eq!(
        sessions
            .find_by_token_hash(&alpha, "hash-1")
            .await
            .unwrap()
            .unwrap()
            .user_id,
        "u1"
    );
    assert!(
        sessions
            .find_by_token_hash(&alpha, "nope")
            .await
            .unwrap()
            .is_none()
    );

    // THE ISOLATION INVARIANT: a session minted for alpha does not exist for
    // beta. A stolen or misdirected cookie cannot cross companies, because
    // there is no row to find — not because a check rejected it.
    assert!(
        sessions
            .find_by_token_hash(&beta, "hash-1")
            .await
            .unwrap()
            .is_none(),
        "a session for alpha must be invisible to beta"
    );

    // Per-user listing, newest first, scoped to the company.
    assert_eq!(sessions.list_for_user(&alpha, "u1").await.unwrap().len(), 2);
    assert!(
        sessions
            .list_for_user(&beta, "u1")
            .await
            .unwrap()
            .is_empty()
    );

    // Single revocation.
    assert!(sessions.delete(&alpha, "s1").await.unwrap());
    assert!(!sessions.delete(&alpha, "s1").await.unwrap());
    assert!(
        sessions
            .find_by_token_hash(&alpha, "hash-1")
            .await
            .unwrap()
            .is_none()
    );

    // Revoking a user drops every session they hold — the lever behind
    // suspend/remove.
    assert_eq!(sessions.delete_for_user(&alpha, "u1").await.unwrap(), 1);
    assert!(
        sessions
            .list_for_user(&alpha, "u1")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(sessions.delete_for_user(&alpha, "u1").await.unwrap(), 0);
    // u2 is untouched.
    assert_eq!(sessions.list_for_user(&alpha, "u2").await.unwrap().len(), 1);

    // Expiry purging drops only what has actually expired. Expiry is exclusive,
    // so a session expiring exactly at `now` is already dead.
    sessions
        .create(&alpha, &session("s4", "hash-4", "u3", 50))
        .await
        .unwrap();
    assert_eq!(sessions.purge_expired(&alpha, 50).await.unwrap(), 1);
    assert!(
        sessions
            .find_by_token_hash(&alpha, "hash-4")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .find_by_token_hash(&alpha, "hash-3")
            .await
            .unwrap()
            .is_some(),
        "a live session must survive a purge"
    );
}

/// Asserts the [`LoginCodeStore`] contract: per-company isolation and — the
/// point of the port — atomic single-use redemption.
pub async fn assert_login_code_store(codes: Arc<dyn LoginCodeStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let code = |id: &str, hash: &str, email: &str, expires: u64| LoginCodeRecord {
        id: id.to_string(),
        code_hash: hash.to_string(),
        email: email.to_string(),
        created_at_millis: 1,
        expires_at_millis: expires,
        consumed_at_millis: None,
    };

    codes
        .create(&alpha, &code("c1", "hash-1", "ada@example.com", 100))
        .await
        .unwrap();

    // A code mailed for alpha must not authenticate against beta.
    assert!(
        codes.consume(&beta, "hash-1", 10).await.unwrap().is_none(),
        "a login code for alpha must be invisible to beta"
    );

    // Redemption returns the record, and binds the session to the address the
    // code was mailed to — not one supplied by the redeemer.
    let consumed = codes
        .consume(&alpha, "hash-1", 10)
        .await
        .unwrap()
        .expect("a live code redeems");
    assert_eq!(consumed.email, "ada@example.com");

    // SINGLE USE: the second redemption of the same code returns nothing. This
    // is what stops a forwarded or replayed magic link from minting a second
    // session.
    assert!(
        codes.consume(&alpha, "hash-1", 11).await.unwrap().is_none(),
        "a code must redeem exactly once"
    );

    // An unknown hash is indistinguishable from a spent one.
    assert!(codes.consume(&alpha, "nope", 10).await.unwrap().is_none());

    // --- latest_for_email: what the resend throttle asks ---
    // Isolation holds here too.
    assert!(
        codes
            .latest_for_email(&beta, "ada@example.com")
            .await
            .unwrap()
            .is_none()
    );
    // A spent code is still the latest one — the throttle asks "when did we
    // last mail this address", not "is there a live code".
    assert_eq!(
        codes
            .latest_for_email(&alpha, "ada@example.com")
            .await
            .unwrap()
            .expect("the spent code is still on record")
            .id,
        "c1"
    );
    assert!(
        codes
            .latest_for_email(&alpha, "nobody@example.com")
            .await
            .unwrap()
            .is_none()
    );
    // With several codes for one address, the most recent wins.
    codes
        .create(&alpha, &code("older", "hash-old", "zoe@example.com", 60))
        .await
        .unwrap();
    let mut newer = code("newer", "hash-new", "zoe@example.com", 200);
    newer.created_at_millis = 50;
    codes.create(&alpha, &newer).await.unwrap();
    assert_eq!(
        codes
            .latest_for_email(&alpha, "zoe@example.com")
            .await
            .unwrap()
            .unwrap()
            .id,
        "newer"
    );
    codes
        .delete_for_email(&alpha, "zoe@example.com")
        .await
        .unwrap();

    // Expiry is exclusive and enforced at redemption, not just at read.
    codes
        .create(&alpha, &code("c2", "hash-2", "bob@example.com", 50))
        .await
        .unwrap();
    assert!(
        codes.consume(&alpha, "hash-2", 50).await.unwrap().is_none(),
        "an expired code must not redeem"
    );

    // Issuing a new code invalidates any outstanding one for that address.
    codes
        .create(&alpha, &code("c3", "hash-3", "carol@example.com", 100))
        .await
        .unwrap();
    assert_eq!(
        codes
            .delete_for_email(&alpha, "carol@example.com")
            .await
            .unwrap(),
        1
    );
    assert!(codes.consume(&alpha, "hash-3", 10).await.unwrap().is_none());
    assert_eq!(
        codes
            .delete_for_email(&alpha, "carol@example.com")
            .await
            .unwrap(),
        0
    );

    // Purging drops expired codes and leaves live ones. At this point the store
    // still holds c1 (spent, expires 100) and c2 (expires 50) alongside the two
    // created here, so purging at 100 collects c1, c2, and c4 — every code whose
    // expiry has passed, spent or not — and spares only c5.
    codes
        .create(&alpha, &code("c4", "hash-4", "dave@example.com", 20))
        .await
        .unwrap();
    codes
        .create(&alpha, &code("c5", "hash-5", "erin@example.com", 200))
        .await
        .unwrap();
    assert_eq!(codes.purge_expired(&alpha, 100).await.unwrap(), 3);
    assert_eq!(
        codes.purge_expired(&alpha, 100).await.unwrap(),
        0,
        "purging twice must not double-count"
    );
    assert!(
        codes
            .consume(&alpha, "hash-5", 100)
            .await
            .unwrap()
            .is_some(),
        "a live code must survive a purge"
    );
}

/// Asserts the [`ArtifactStore`] contract: isolation, per-task filtering,
/// version-history round-trip, upsert, and delete.
///
/// The version-history assertion is the load-bearing one. An artifact's whole
/// value is that nothing is overwritten — so a backend that stored only the
/// latest body, or that reordered versions, would still pass a naive
/// "upsert then read back the title" check while destroying the human-edit
/// diff. This asserts the full ordered history survives the round-trip.
pub async fn assert_artifact_store(artifacts: Arc<dyn ArtifactStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    let mut draft = ArtifactRecord::new(
        "a1",
        "t-1",
        "Launch post",
        ArtifactKind::Markdown,
        "agent draft",
        "ceo",
        1,
    );
    draft.push_version(
        "operator polish",
        ArtifactAuthor::Operator,
        "operator",
        2,
        Some("operator edit before approval".to_string()),
    );
    artifacts.upsert(&alpha, &draft).await.unwrap();

    let other_task =
        ArtifactRecord::new("a2", "t-2", "Spec", ArtifactKind::Text, "notes", "ceo", 3);
    artifacts.upsert(&alpha, &other_task).await.unwrap();

    let leak = ArtifactRecord::new(
        "b1",
        "t-1",
        "Secret",
        ArtifactKind::Text,
        "hidden",
        "ceo",
        4,
    );
    artifacts.upsert(&beta, &leak).await.unwrap();

    // Isolation: company beta's artifact is invisible to alpha, including under
    // the same task id.
    assert_eq!(artifacts.list(&alpha, None).await.unwrap().len(), 2);
    assert_eq!(artifacts.list(&beta, None).await.unwrap().len(), 1);
    let alpha_t1 = artifacts.list(&alpha, Some("t-1")).await.unwrap();
    assert_eq!(alpha_t1.len(), 1, "task filter narrows to one card");
    assert_eq!(alpha_t1[0].id, "a1");

    // The full ordered version history round-trips, authors intact.
    let back = artifacts
        .get(&alpha, "a1")
        .await
        .unwrap()
        .expect("a1 exists");
    assert_eq!(back, draft, "the whole record must round-trip verbatim");
    assert_eq!(back.versions.len(), 2);
    assert_eq!(back.versions[0].version, 1);
    assert_eq!(back.versions[0].author, ArtifactAuthor::Agent);
    assert_eq!(back.versions[0].body, "agent draft");
    assert_eq!(back.versions[1].author, ArtifactAuthor::Operator);
    // …and therefore the human-edit diff is still computable after a round-trip.
    let diff = back.human_edit_diff().expect("an operator edited");
    assert_eq!((diff.from_version, diff.to_version), (1, 2));

    // A missing id reads as `None`, not an error.
    assert!(artifacts.get(&alpha, "nope").await.unwrap().is_none());

    // Upsert replaces last-write-wins.
    let mut revised = back;
    revised.push_version("third pass", ArtifactAuthor::Agent, "ceo", 9, None);
    artifacts.upsert(&alpha, &revised).await.unwrap();
    let after = artifacts.get(&alpha, "a1").await.unwrap().unwrap();
    assert_eq!(after.versions.len(), 3);
    assert_eq!(after.latest().unwrap().version, 3);
    assert_eq!(
        artifacts.list(&alpha, None).await.unwrap().len(),
        2,
        "upsert replaces, never duplicates"
    );

    // ── Issue #244: `source` is what an artifact is an artifact *of* ────────

    // It survives the round trip on every backend. All three persist the record
    // as an opaque JSON blob, so this is really asserting the blob is opaque —
    // a backend that projected named columns would silently drop it.
    let spec = ArtifactRecord::new(
        "a3",
        "t-3",
        "Launch spec",
        ArtifactKind::Markdown,
        "# Spec",
        "ceo",
        10,
    )
    .with_source("specs/launch.md");
    artifacts.upsert(&alpha, &spec).await.unwrap();
    let back = artifacts.get(&alpha, "a3").await.unwrap().expect("a3");
    assert_eq!(back.source.as_deref(), Some("specs/launch.md"));
    assert_eq!(
        back, spec,
        "the whole record must still round-trip verbatim"
    );

    // A record written before #244 loads with `source: None` rather than
    // failing — which is what marks it as legacy reply capture. `a2` above was
    // built through `ArtifactRecord::new`, i.e. exactly the pre-#244 shape.
    assert_eq!(
        artifacts
            .get(&alpha, "a2")
            .await
            .unwrap()
            .expect("a2")
            .source,
        None
    );

    // Two different files on ONE card coexist as separate records. This is the
    // shape identity exists for: without it, the second publish would have to
    // either duplicate or overwrite, and the human-edit diff of whichever it
    // hit would stop meaning anything.
    let invoice = ArtifactRecord::new(
        "a4",
        "t-3",
        "Invoice",
        ArtifactKind::Markdown,
        "# Invoice",
        "ceo",
        11,
    )
    .with_source("billing/invoice.md");
    artifacts.upsert(&alpha, &invoice).await.unwrap();
    let on_card = artifacts.list(&alpha, Some("t-3")).await.unwrap();
    assert_eq!(on_card.len(), 2, "one card, two deliverables");
    let mut sources: Vec<&str> = on_card.iter().filter_map(|a| a.source.as_deref()).collect();
    sources.sort_unstable();
    assert_eq!(sources, ["billing/invoice.md", "specs/launch.md"]);

    // Delete reports whether anything went, and does not touch the sibling.
    assert!(artifacts.delete(&alpha, "a1").await.unwrap());
    assert!(!artifacts.delete(&alpha, "a1").await.unwrap());
    assert_eq!(artifacts.list(&alpha, None).await.unwrap().len(), 3);
    assert_eq!(artifacts.list(&beta, None).await.unwrap().len(), 1);
}

/// Asserts the [`WorkflowRevisionStore`] contract (issue #274): per-company and
/// per-workflow isolation, newest-first order, prune-to-cap inside the push, a
/// verbatim body round-trip, and the delete cascade.
///
/// The prune assertion is the load-bearing one: a backend that grew the ring
/// unbounded, or that pruned the *newest* rows instead of the oldest, would
/// still pass a naive "push then read back" check while defeating the whole
/// point of a bounded history.
pub async fn assert_workflow_revision_store(revisions: Arc<dyn WorkflowRevisionStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    // A helper that pins the capture time, so ordering is asserted against a
    // known sequence rather than the wall clock.
    let rev = |workflow_id: &str, name: &str, toml: &str, at: u64| {
        let mut r = WorkflowRevisionRecord::new(workflow_id, name, toml, at);
        // Force a distinct, ordered id so the tie-break is deterministic even
        // when two share a millisecond.
        r.id = format!("{workflow_id}-{at:04}");
        r
    };

    // Two revisions of `greeter`, plus one of a sibling workflow, plus one under
    // company beta that must never leak into alpha.
    let first = rev(
        "greeter",
        "Greeter v1",
        "id = \"greeter\"\nname = \"Greeter v1\"",
        10,
    );
    let second = rev(
        "greeter",
        "Greeter v2",
        "id = \"greeter\"\nname = \"Greeter v2\"",
        20,
    );
    revisions.push_revision(&alpha, &first).await.unwrap();
    revisions.push_revision(&alpha, &second).await.unwrap();
    revisions
        .push_revision(&alpha, &rev("digest", "Digest", "id = \"digest\"", 15))
        .await
        .unwrap();
    revisions
        .push_revision(&beta, &rev("greeter", "Other", "id = \"greeter\"", 99))
        .await
        .unwrap();

    // Isolation by company AND by workflow.
    let alpha_greeter = revisions.list_revisions(&alpha, "greeter").await.unwrap();
    assert_eq!(alpha_greeter.len(), 2, "only greeter's two snapshots");
    assert_eq!(
        revisions
            .list_revisions(&alpha, "digest")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        revisions
            .list_revisions(&beta, "greeter")
            .await
            .unwrap()
            .len(),
        1
    );

    // Newest first, and the body round-trips verbatim.
    assert_eq!(alpha_greeter[0].id, second.id, "newest snapshot leads");
    assert_eq!(alpha_greeter[1].id, first.id);
    assert_eq!(
        alpha_greeter[0].toml, second.toml,
        "the captured TOML must survive byte-for-byte"
    );

    // get_revision is workflow-scoped: greeter's id is invisible under digest.
    assert!(
        revisions
            .get_revision(&alpha, "greeter", &first.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        revisions
            .get_revision(&alpha, "digest", &first.id)
            .await
            .unwrap()
            .is_none(),
        "a revision id must not resolve under the wrong workflow"
    );
    assert!(
        revisions
            .get_revision(&alpha, "greeter", "nope")
            .await
            .unwrap()
            .is_none()
    );

    // Prune-to-cap: push MAX+5 distinct snapshots of a fresh workflow and prove
    // the ring holds exactly MAX, keeping the newest and dropping the oldest.
    for i in 0..(MAX_WORKFLOW_REVISIONS as u64 + 5) {
        revisions
            .push_revision(
                &alpha,
                &rev(
                    "ring",
                    &format!("v{i}"),
                    &format!("id = \"ring\" # {i}"),
                    1000 + i,
                ),
            )
            .await
            .unwrap();
    }
    let ring = revisions.list_revisions(&alpha, "ring").await.unwrap();
    assert_eq!(
        ring.len(),
        MAX_WORKFLOW_REVISIONS,
        "the ring must be capped at MAX_WORKFLOW_REVISIONS"
    );
    assert_eq!(
        ring[0].created_at_millis,
        1000 + MAX_WORKFLOW_REVISIONS as u64 + 4,
        "the newest snapshot survives the prune"
    );
    assert_eq!(
        ring[ring.len() - 1].created_at_millis,
        1000 + 5,
        "the oldest kept is exactly MAX back from the newest — older ones pruned"
    );

    // The prune must not have touched the sibling workflows or company beta.
    assert_eq!(
        revisions
            .list_revisions(&alpha, "greeter")
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        revisions
            .list_revisions(&beta, "greeter")
            .await
            .unwrap()
            .len(),
        1
    );

    // Delete cascade: drops exactly one workflow's history, reports the count,
    // and is a no-op the second time.
    let removed = revisions.delete_revisions(&alpha, "greeter").await.unwrap();
    assert_eq!(removed, 2, "both greeter snapshots removed");
    assert!(
        revisions
            .list_revisions(&alpha, "greeter")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        revisions.delete_revisions(&alpha, "greeter").await.unwrap(),
        0
    );
    // Siblings and beta untouched by the cascade.
    assert_eq!(
        revisions
            .list_revisions(&alpha, "digest")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        revisions
            .list_revisions(&alpha, "ring")
            .await
            .unwrap()
            .len(),
        MAX_WORKFLOW_REVISIONS
    );
    assert_eq!(
        revisions
            .list_revisions(&beta, "greeter")
            .await
            .unwrap()
            .len(),
        1
    );
}

/// Asserts the [`WorkflowRunOutputStore`] contract (issue #596): roundtrip,
/// company isolation, overwrite idempotence, and prune-to-newest-N.
pub async fn assert_workflow_run_output_store(outputs: Arc<dyn WorkflowRunOutputStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    let record = |run_id: &str, workflow_id: &str, at: u64, marker: &str| WorkflowRunOutputRecord {
        run_id: run_id.to_string(),
        workflow_id: workflow_id.to_string(),
        at_millis: at,
        nodes: serde_json::json!({ "writer": { "items": [marker] } }),
        truncated: false,
        partial: false,
    };

    // Roundtrip: a stored record reads back byte-identically.
    let first = record("run-1", "greet", 10, "hello");
    outputs.put_run_output(&alpha, &first).await.unwrap();
    let got = outputs
        .get_run_output(&alpha, "run-1")
        .await
        .unwrap()
        .expect("the stored run output must read back");
    assert_eq!(got, first, "a run output must round-trip verbatim");

    // A run that was never stored is `None`, not an error — the pre-feature /
    // dry-run / hard-abort shape the read route turns into a 404.
    assert!(
        outputs
            .get_run_output(&alpha, "never")
            .await
            .unwrap()
            .is_none(),
        "an unknown run id must read back as None"
    );

    // Company isolation: beta cannot see alpha's run, even by the same id.
    outputs
        .put_run_output(&beta, &record("run-1", "greet", 10, "beta-secret"))
        .await
        .unwrap();
    let beta_got = outputs
        .get_run_output(&beta, "run-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(beta_got.nodes["writer"]["items"][0], "beta-secret");
    let alpha_got = outputs
        .get_run_output(&alpha, "run-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        alpha_got.nodes["writer"]["items"][0], "hello",
        "one company's run output must never leak into another's"
    );

    // Overwrite idempotence: re-writing the same run_id replaces, never stacks.
    let replaced = record("run-1", "greet", 20, "world");
    outputs.put_run_output(&alpha, &replaced).await.unwrap();
    let after = outputs
        .get_run_output(&alpha, "run-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after, replaced,
        "a re-write must overwrite the prior snapshot"
    );

    // Prune-to-cap: push MAX+5 distinct runs and prove only the newest MAX survive.
    for i in 0..(MAX_RUN_OUTPUTS_PER_COMPANY as u64 + 5) {
        outputs
            .put_run_output(&alpha, &record(&format!("r{i}"), "ring", 1000 + i, "x"))
            .await
            .unwrap();
    }
    // The newest run is retained…
    let newest_id = format!("r{}", MAX_RUN_OUTPUTS_PER_COMPANY as u64 + 4);
    assert!(
        outputs
            .get_run_output(&alpha, &newest_id)
            .await
            .unwrap()
            .is_some(),
        "the newest run must survive the prune"
    );
    // …and the oldest of the batch was evicted.
    assert!(
        outputs
            .get_run_output(&alpha, "r0")
            .await
            .unwrap()
            .is_none(),
        "the oldest run beyond the cap must be pruned"
    );

    // Beta's single run is untouched by alpha's prune.
    assert!(
        outputs
            .get_run_output(&beta, "run-1")
            .await
            .unwrap()
            .is_some(),
        "one company's prune must not touch another's records"
    );
}

/// Asserts the [`FactStore`] contract: isolation, query/kind filtering, upsert,
/// and delete.
pub async fn assert_fact_store(facts: Arc<dyn FactStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let fact = |id: &str, kind: FactKind, title: &str, body: &str, at: u64| FactRecord {
        id: id.to_string(),
        kind,
        title: title.to_string(),
        body: body.to_string(),
        source: "You".to_string(),
        updated_at_millis: at,
    };

    facts
        .upsert(
            &alpha,
            &fact("f1", FactKind::Preference, "Tone", "Warm and direct", 1),
        )
        .await
        .unwrap();
    facts
        .upsert(
            &alpha,
            &fact("f2", FactKind::Person, "Dana", "Lead designer", 2),
        )
        .await
        .unwrap();
    facts
        .upsert(&beta, &fact("b1", FactKind::Fact, "Leak", "secret", 3))
        .await
        .unwrap();

    // Isolation.
    assert_eq!(facts.list(&beta, None, None).await.unwrap().len(), 1);

    // Kind filter.
    let people = facts
        .list(&alpha, None, Some(FactKind::Person))
        .await
        .unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].id, "f2");

    // Query filter over title + body (case-insensitive).
    let hits = facts.list(&alpha, Some("designer"), None).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "f2");

    // Upsert replaces last-write-wins.
    facts
        .upsert(
            &alpha,
            &fact("f1", FactKind::Preference, "Tone", "Playful", 9),
        )
        .await
        .unwrap();
    let all = facts.list(&alpha, None, None).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, "f1", "newest-first");
    assert_eq!(all.iter().find(|f| f.id == "f1").unwrap().body, "Playful");

    // Delete + journaling is the caller's job; the store just removes.
    assert!(facts.delete(&alpha, "f1").await.unwrap());
    assert!(!facts.delete(&alpha, "f1").await.unwrap());
    assert_eq!(facts.list(&alpha, None, None).await.unwrap().len(), 1);
}

/// Asserts every [`ContextStore`] stamps a stored chunk with the wall-clock
/// time it was written, and surfaces that stamp through `list`.
///
/// This is what lets the Brain's "Last updated" stat move when agents write
/// memory: agent memory and task outcomes only ever land in the `ContextStore`,
/// so without a per-chunk stamp the stat can only reflect operator-authored
/// facts (see `server::ops::memory::memory_stats`).
///
/// Deliberately says nothing about a re-`put`'s effect on the stamp: every
/// backend keeps one claim per (addr, label) since #1300 (see
/// [`assert_identical_body_two_labels`]), but a *new* label on an existing
/// body stamps per-label on fs/sqlite and keeps the address's first-write
/// stamp on the single-record backends (mongodb, the provider facade, the
/// tinycortex engine). Readers of the stamp take the max across chunks for
/// that reason.
pub async fn assert_context_chunk_stamps(context: Arc<dyn ContextStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let before = now_millis();

    context
        .put(
            &alpha,
            ContextChunk {
                label: "agent/ceo".to_string(),
                body: "the launch slipped to Friday".to_string(),
            },
        )
        .await
        .unwrap();
    let after = now_millis();

    let metas = context.list(&alpha, "").await.unwrap();
    assert_eq!(metas.len(), 1);
    let stamped = metas[0].stored_at_millis;
    assert!(
        (before..=after).contains(&stamped),
        "a stored chunk must carry the time it was written; got {stamped}, expected within \
         {before}..={after}"
    );

    // The stamp travels per company, like every other field on the port.
    assert!(context.list(&beta, "").await.unwrap().is_empty());
}

/// Asserts [`ContextStore::peek_many`] answers positionally: one entry per
/// requested addr, in request order, `None` where nothing is stored — the
/// contract the default loop-of-peeks gives and every bulk-read override must
/// preserve exactly.
pub async fn assert_context_peek_many_answers_positionally(context: Arc<dyn ContextStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    let first = context
        .put(
            &alpha,
            ContextChunk {
                label: "agent/one".to_string(),
                body: "first body".to_string(),
            },
        )
        .await
        .unwrap();
    let second = context
        .put(
            &alpha,
            ContextChunk {
                label: "agent/two".to_string(),
                body: "second body".to_string(),
            },
        )
        .await
        .unwrap();

    // Out of storage order, with a hole in the middle: the answer must follow
    // the REQUEST order and report the hole as `None`, not shift or error.
    let missing = ChunkAddr::new("no-such-addr");
    let bodies = context
        .peek_many(&alpha, &[second.clone(), missing, first.clone()])
        .await
        .unwrap();
    assert_eq!(
        bodies,
        vec![
            Some("second body".to_string()),
            None,
            Some("first body".to_string()),
        ],
        "one answer per requested addr, positionally"
    );

    // An empty batch is a no-op, never an error.
    assert_eq!(
        context.peek_many(&alpha, &[]).await.unwrap(),
        Vec::<Option<String>>::new()
    );

    // Addresses answer per company, like every other read on the port.
    assert_eq!(
        context.peek_many(&beta, &[first]).await.unwrap(),
        vec![None],
        "another company's addr must not leak a body"
    );
}

/// A multibyte char near a match must not panic the search snippet's ±24-byte
/// window, and a ranged `peek` whose bounds land mid-codepoint must widen to
/// the char boundary rather than panic. `memory_recall` routes agent queries
/// straight into `search`, so the panic would be reachable from any non-ASCII
/// chunk body.
pub async fn assert_multibyte_bodies_survive_search_and_ranged_peek(
    context: Arc<dyn ContextStore>,
) {
    let alpha = CompanyId::new("alpha");
    // Thirteen two-byte chars directly before the match, so the snippet
    // window's `pos - 24` lands mid-codepoint.
    let body = "ééééééééééééé match target";
    let addr = context
        .put(
            &alpha,
            ContextChunk {
                label: "agent/multibyte".to_string(),
                body: body.to_string(),
            },
        )
        .await
        .unwrap();

    let hits = context.search(&alpha, "match", usize::MAX).await.unwrap();
    assert_eq!(hits.len(), 1, "the multibyte body must hit, not panic");
    assert!(
        hits[0].snippet.contains("match"),
        "the snippet lost the match: {:?}",
        hits[0].snippet
    );

    // A range ending inside the first "é" widens to its boundary.
    assert_eq!(context.peek(&alpha, &addr, Some(0..1)).await.unwrap(), "é");
}

/// Asserts byte-identical bodies under two labels both land (issue #1300):
/// one content address, one claim per (addr, label), on every backend.
///
/// Before #1300 the backends diverged exactly here — the fs index appended a
/// row per put (including duplicates), while sqlite/mongodb/the provider
/// facade were first-write-wins on the address, answering a success receipt
/// for a second label that never landed. A caller listing by their label then
/// found nothing on those backends and everything on fs, and nothing pinned
/// either behaviour because every other case in this suite uses distinct
/// bodies.
pub async fn assert_identical_body_two_labels(context: Arc<dyn ContextStore>) {
    let company = CompanyId::new("alpha");
    let twin = "twin body: byte-identical under two labels";
    let first = context
        .put(
            &company,
            ContextChunk {
                label: "labels/first".to_string(),
                body: twin.to_string(),
            },
        )
        .await
        .unwrap();
    let second = context
        .put(
            &company,
            ContextChunk {
                label: "labels/second".to_string(),
                body: twin.to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(first, second, "identical bodies share one content address");

    let labels_at = |metas: &[ChunkMeta]| -> Vec<String> {
        metas
            .iter()
            .filter(|m| m.addr == first)
            .map(|m| m.label.clone())
            .collect()
    };
    let mut labels = labels_at(&context.list(&company, "labels/").await.unwrap());
    labels.sort();
    assert_eq!(
        labels,
        ["labels/first", "labels/second"],
        "both labels must claim the shared address"
    );

    // And each label's claim is findable through its own prefix — the exact
    // read that used to answer empty on the first-write-wins backends.
    let second_only = context.list(&company, "labels/second").await.unwrap();
    assert_eq!(labels_at(&second_only), ["labels/second"]);

    // Set semantics: a re-put of an identical (body, label) adds nothing.
    context
        .put(
            &company,
            ContextChunk {
                label: "labels/first".to_string(),
                body: twin.to_string(),
            },
        )
        .await
        .unwrap();
    let again = labels_at(&context.list(&company, "labels/").await.unwrap());
    assert_eq!(
        again
            .iter()
            .filter(|l| l.as_str() == "labels/first")
            .count(),
        1,
        "a re-put of an identical (body, label) must not duplicate the claim: {again:?}"
    );
}

/// Asserts [`ContextStore::delete_label`] removes one claim, reaps the body
/// with the last claim, and answers `false` for claims that are not there
/// (issue #1300) — and that address-level [`ContextStore::delete`] still
/// takes every claim at once.
pub async fn assert_delete_label_scoped(context: Arc<dyn ContextStore>) {
    let company = CompanyId::new("alpha");
    let body = "shared claim body for the label-scoped delete";
    let addr = context
        .put(
            &company,
            ContextChunk {
                label: "scoped/mine".to_string(),
                body: body.to_string(),
            },
        )
        .await
        .unwrap();
    context
        .put(
            &company,
            ContextChunk {
                label: "scoped/theirs".to_string(),
                body: body.to_string(),
            },
        )
        .await
        .unwrap();

    assert!(
        !context
            .delete_label(&company, &addr, "scoped/absent")
            .await
            .unwrap(),
        "an absent label answers false"
    );
    assert!(
        context
            .delete_label(&company, &addr, "scoped/mine")
            .await
            .unwrap()
    );
    assert!(
        !context
            .delete_label(&company, &addr, "scoped/mine")
            .await
            .unwrap(),
        "a second delete of the same claim answers false"
    );

    let labels: Vec<String> = context
        .list(&company, "scoped/")
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.addr == addr)
        .map(|m| m.label)
        .collect();
    assert_eq!(
        labels,
        ["scoped/theirs"],
        "only the named label's claim goes"
    );
    assert_eq!(
        context.peek(&company, &addr, None).await.unwrap(),
        body,
        "the body survives while any label claims it"
    );

    assert!(
        context
            .delete_label(&company, &addr, "scoped/theirs")
            .await
            .unwrap()
    );
    assert!(
        context
            .list(&company, "scoped/")
            .await
            .unwrap()
            .iter()
            .all(|m| m.addr != addr),
        "no claim may remain after the last label goes"
    );
    assert!(
        context.peek(&company, &addr, None).await.is_err(),
        "the body is reaped with its last claim"
    );
    assert!(
        !context
            .delete_label(&company, &addr, "scoped/theirs")
            .await
            .unwrap(),
        "a fully-reaped address answers false"
    );

    // Address-level delete still takes every claim at once — the operator
    // semantics `delete_label` deliberately is not.
    let addr = context
        .put(
            &company,
            ContextChunk {
                label: "scoped/a".to_string(),
                body: "whole-address body".to_string(),
            },
        )
        .await
        .unwrap();
    context
        .put(
            &company,
            ContextChunk {
                label: "scoped/b".to_string(),
                body: "whole-address body".to_string(),
            },
        )
        .await
        .unwrap();
    assert!(context.delete(&company, &addr).await.unwrap());
    assert!(
        context
            .list(&company, "scoped/")
            .await
            .unwrap()
            .iter()
            .all(|m| m.addr != addr),
        "address-level delete takes every label's claim"
    );
    assert!(!context.delete(&company, &addr).await.unwrap());
}

/// Asserts a `delete_label` cannot lose a byte-identical write that lands
/// beside it (issue #1300's TOCTOU half).
///
/// The defect this locks: both shared-address guards used to read a snapshot,
/// decide nothing else claimed the address, and then delete it — so a write
/// of identical content under another label landing in that window lost its
/// row. `delete_label` closes it *by construction* only if each backend makes
/// the claim-removal and the last-claim reap one atomic step (a lock, a
/// transaction, or a conditional delete). This drives both operations
/// concurrently, many times, and demands the second writer's claim and body
/// survive every round.
///
/// What it can and cannot prove, stated plainly: the backends that serialise
/// the two calls against each other (fs on its index lock, sqlite on its
/// connection, the facade and the engine on their own locks) satisfy this
/// whatever the interleaving, so here it is a **regression lock** — it fails
/// if someone removes that serialisation. Only mongodb runs the two against a
/// server that can genuinely interleave them, and it is the backend whose
/// atomicity rests on a conditional delete rather than a lock, which is the
/// case most worth driving. The tasks are spawned rather than awaited in
/// order, so they interleave at every `.await` even on a current-thread
/// runtime.
pub async fn assert_delete_label_survives_a_concurrent_identical_put(
    context: Arc<dyn ContextStore>,
) {
    let company = CompanyId::new("alpha");
    // Each round uses a fresh body, so a round can never be satisfied by the
    // previous round's surviving row.
    for round in 0..32 {
        let body = format!("racing body {round}");
        let addr = context
            .put(
                &company,
                ContextChunk {
                    label: "race/first".to_string(),
                    body: body.clone(),
                },
            )
            .await
            .unwrap();

        let deleter = {
            let context = Arc::clone(&context);
            let company = company.clone();
            let addr = addr.clone();
            tokio::spawn(async move { context.delete_label(&company, &addr, "race/first").await })
        };
        let writer = {
            let context = Arc::clone(&context);
            let company = company.clone();
            let body = body.clone();
            tokio::spawn(async move {
                context
                    .put(
                        &company,
                        ContextChunk {
                            label: "race/second".to_string(),
                            body,
                        },
                    )
                    .await
            })
        };
        deleter.await.unwrap().unwrap();
        writer.await.unwrap().unwrap();

        let labels: Vec<String> = context
            .list(&company, "race/")
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.addr == addr)
            .map(|m| m.label)
            .collect();
        assert!(
            labels.iter().any(|label| label == "race/second"),
            "round {round}: the concurrent writer's claim was lost to the delete: {labels:?}"
        );
        assert_eq!(
            context.peek(&company, &addr, None).await.unwrap(),
            body,
            "round {round}: the body was reaped while a claim still held it"
        );

        // Leave nothing behind for the next round.
        context.delete(&company, &addr).await.unwrap();
    }
}

/// Asserts the [`UsageMeter`] contract: isolation, record, and windowed query.
pub async fn assert_usage_meter(usage: Arc<dyn UsageMeter>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let sample = |at: u64, cost: f64| UsageSample {
        at_millis: at,
        agent: "ceo".to_string(),
        provider: "managed".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        cached_input_tokens: 10,
        cost_usd: cost,
        kind: SampleKind::Inference,
        run_id: None,
    };

    usage.record(&alpha, &sample(100, 0.1)).await.unwrap();
    usage.record(&alpha, &sample(200, 0.2)).await.unwrap();
    usage.record(&beta, &sample(150, 9.9)).await.unwrap();

    // Isolation.
    assert_eq!(usage.query(&beta, 0).await.unwrap().len(), 1);

    // Full window, oldest first.
    let all = usage.query(&alpha, 0).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].at_millis, 100);
    assert_eq!(all[1].at_millis, 200);

    // Windowed query honours the `since` lower bound.
    let recent = usage.query(&alpha, 150).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].at_millis, 200);
    assert_eq!(recent[0].kind, SampleKind::Inference);

    // Issue #337: a `PlanningCall` sample round-trips on every backend, kind
    // and attribution intact.
    //
    // The kind is what the Usage view will one day separate planning spend by,
    // and the agent is the whole cost decision — a backend that dropped either
    // would silently re-attribute planning to whatever bucket the reader
    // defaulted to. `agent: "company"` is asserted against the literal rather
    // than the constant on purpose: it is a *stored* value, so a rename of the
    // constant must not silently re-file every historical sample.
    let planning = UsageSample {
        at_millis: 300,
        agent: crate::metering::UNATTRIBUTED_AGENT.to_string(),
        provider: "managed".to_string(),
        input_tokens: 900,
        output_tokens: 300,
        cached_input_tokens: 100,
        cost_usd: 0.03,
        kind: SampleKind::PlanningCall,
        run_id: None,
    };
    usage.record(&alpha, &planning).await.unwrap();
    let back = usage
        .query(&alpha, 300)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.kind == SampleKind::PlanningCall)
        .expect("the planning sample persists");
    assert_eq!(back, planning);
    assert_eq!(back.agent, "company");
    assert!(back.run_id.is_none(), "a planning pass has no attempt row");
}

/// Asserts the [`UsageMeter`] retention contract: samples older than the 90-day
/// window are evicted on write, anchored to the newest sample recorded.
pub async fn assert_usage_retention(usage: Arc<dyn UsageMeter>) {
    use crate::ports::usage::RETENTION_MILLIS;

    let acme = CompanyId::new("acme");
    let sample = |at: u64| UsageSample {
        at_millis: at,
        agent: "ceo".to_string(),
        provider: "managed".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        cached_input_tokens: 10,
        cost_usd: 0.1,
        kind: SampleKind::Inference,
        run_id: None,
    };

    // A fixed base far from epoch 0 so the cutoff math stays positive.
    let base: u64 = 1_000_000_000_000;
    let stale = base;
    let boundary = base + RETENTION_MILLIS; // exactly 90 days newer — kept.
    let fresh = base + RETENTION_MILLIS + 86_400_000; // 91 days newer.

    // Seed a stale sample, then a boundary sample: nothing evicted yet (the
    // newest is only 90 days ahead of the stale one).
    usage.record(&acme, &sample(stale)).await.unwrap();
    usage.record(&acme, &sample(boundary)).await.unwrap();
    let all = usage.query(&acme, 0).await.unwrap();
    assert_eq!(
        all.len(),
        2,
        "boundary write keeps the exactly-90d-old sample"
    );

    // A fresh write pushes the cutoff past the stale sample, evicting it.
    usage.record(&acme, &sample(fresh)).await.unwrap();
    let kept = usage.query(&acme, 0).await.unwrap();
    let ats: Vec<u64> = kept.iter().map(|s| s.at_millis).collect();
    assert!(!ats.contains(&stale), "stale sample evicted: {ats:?}");
    assert!(ats.contains(&boundary), "boundary sample retained: {ats:?}");
    assert!(ats.contains(&fresh), "fresh sample retained: {ats:?}");
}

/// Asserts the [`SkillStateStore`] contract: isolation, set/upsert, and remove.
/// Every [`ReadStateStore`] must agree on these (issue #755).
///
/// The two properties worth pinning are the ones a backend can plausibly get
/// wrong: **monotonicity** (a marker never moves backwards, however requests
/// interleave) and **isolation** (per person as well as per company — a marker
/// keyed only by channel would let one operator's reading clear another's
/// badges).
pub async fn assert_read_state_store(reads: Arc<dyn crate::ports::read_state::ReadStateStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    // A channel never opened has no marker — absent, not zero.
    assert!(reads.list(&alpha, "ada").await.unwrap().is_empty());

    let first = reads
        .mark(&alpha, "ada", "engineering", 1_000)
        .await
        .unwrap();
    assert_eq!(first.last_read_at, 1_000);
    assert_eq!(first.channel_id, "engineering");

    // Forward moves.
    assert_eq!(
        reads
            .mark(&alpha, "ada", "engineering", 2_000)
            .await
            .unwrap()
            .last_read_at,
        2_000
    );

    // Backwards does NOT. A late request carrying an earlier instant must not
    // resurrect messages already read, and the call answers with where the
    // marker actually stands rather than what was asked for.
    assert_eq!(
        reads
            .mark(&alpha, "ada", "engineering", 500)
            .await
            .unwrap()
            .last_read_at,
        2_000
    );
    // Equal is not a move either, and must not error.
    assert_eq!(
        reads
            .mark(&alpha, "ada", "engineering", 2_000)
            .await
            .unwrap()
            .last_read_at,
        2_000
    );

    // Per person: Grace's marker on the same channel is her own, and reading it
    // to the past does not disturb Ada's.
    reads
        .mark(&alpha, "grace", "engineering", 42)
        .await
        .unwrap();
    let ada = reads.list(&alpha, "ada").await.unwrap();
    assert_eq!(ada.len(), 1);
    assert_eq!(ada[0].last_read_at, 2_000);
    let grace = reads.list(&alpha, "grace").await.unwrap();
    assert_eq!(grace.len(), 1);
    assert_eq!(grace[0].last_read_at, 42);

    // Per company: the same person in another company starts empty.
    assert!(reads.list(&beta, "ada").await.unwrap().is_empty());
    reads.mark(&beta, "ada", "engineering", 9).await.unwrap();
    assert_eq!(
        reads.list(&alpha, "ada").await.unwrap()[0].last_read_at,
        2_000
    );

    // Several channels for one person, ordered by channel id ascending — the
    // order the trait documents, not each backend's insertion order.
    // `engineering` was written first and still sorts second.
    reads.mark(&alpha, "ada", "dm:pm", 7).await.unwrap();
    let all = reads.list(&alpha, "ada").await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].channel_id, "dm:pm");
    assert_eq!(all[1].channel_id, "engineering");
}

/// Asserts the [`NotificationStore`] contract: per-company isolation, per-person
/// read state, newest-first ordering, and the latch / `None`-marks-all
/// semantics of `mark_read` (issue #749).
///
/// The property that matters most is **per-person read state**: one person
/// marking a notification read must leave it unread for another. A `read` flag
/// on the shared record — inbox's shape — would fail exactly this, which is why
/// the port does not have one.
pub async fn assert_notification_store(notes: Arc<dyn NotificationStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    let note = |id: &str, created_at: u64, subject: SubjectKind, subject_id: &str| Notification {
        id: id.to_string(),
        kind: "approval_blocked".to_string(),
        subject: Subject {
            kind: subject,
            id: subject_id.to_string(),
        },
        created_at,
        title: format!("notification {id}"),
    };

    // Empty: nobody has anything.
    assert!(notes.list(&alpha, "ada").await.unwrap().is_empty());

    // Two notifications, appended oldest-then-newest.
    notes
        .append(&alpha, &note("n-old", 100, SubjectKind::Task, "task-1"))
        .await
        .unwrap();
    notes
        .append(&alpha, &note("n-new", 200, SubjectKind::Approval, "appr-1"))
        .await
        .unwrap();

    // Newest first, and unread for everyone until read.
    let ada = notes.list(&alpha, "ada").await.unwrap();
    assert_eq!(ada.len(), 2);
    assert_eq!(ada[0].notification.id, "n-new");
    assert_eq!(ada[1].notification.id, "n-old");
    assert!(ada.iter().all(|v| v.read_at.is_none()));
    // The subject rides through untouched.
    assert_eq!(ada[0].notification.subject.kind, SubjectKind::Approval);
    assert_eq!(ada[0].notification.subject.id, "appr-1");

    // Append is idempotent by id (first write wins): re-appending an existing
    // id neither duplicates the record nor mutates it. Backends must agree — a
    // naive push/insert duplicates on fs/mongo but errors on the sqlite primary
    // key, so this pins the shared contract.
    notes
        .append(&alpha, &note("n-old", 999, SubjectKind::Run, "changed"))
        .await
        .unwrap();
    let after = notes.list(&alpha, "ada").await.unwrap();
    assert_eq!(after.len(), 2, "re-appending an id must not duplicate");
    let old = after.iter().find(|v| v.notification.id == "n-old").unwrap();
    assert_eq!(
        old.notification.created_at, 100,
        "first write wins: created_at unchanged"
    );
    assert_eq!(
        old.notification.subject.id, "task-1",
        "first write wins: subject unchanged"
    );

    // Per person: Ada reads the new one; the count of what is still unread for
    // her comes back, and Grace still sees it unread.
    let still_unread = notes
        .mark_read(&alpha, "ada", Some(&["n-new".to_string()]))
        .await
        .unwrap();
    assert_eq!(still_unread, 1, "n-old is still unread for Ada");

    let ada = notes.list(&alpha, "ada").await.unwrap();
    let stamped = ada
        .iter()
        .find(|v| v.notification.id == "n-new")
        .unwrap()
        .read_at
        .expect("Ada read n-new");
    assert!(
        ada.iter()
            .find(|v| v.notification.id == "n-old")
            .unwrap()
            .read_at
            .is_none()
    );
    let grace = notes.list(&alpha, "grace").await.unwrap();
    assert!(
        grace.iter().all(|v| v.read_at.is_none()),
        "Ada's read must not touch Grace"
    );

    // A latch: re-marking does not move the timestamp forward.
    //
    // Wait past a millisecond boundary first: within one millisecond a backend
    // that OVERWRITES `read_at` on every mark writes the same value a latch
    // would, and this assertion would then pass for the very shape it exists to
    // reject. After the wait, an overwriting backend produces a strictly greater
    // `read_at` (and this fails); a real latch is unchanged (and this holds).
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    notes
        .mark_read(&alpha, "ada", Some(&["n-new".to_string()]))
        .await
        .unwrap();
    let ada = notes.list(&alpha, "ada").await.unwrap();
    assert_eq!(
        ada.iter()
            .find(|v| v.notification.id == "n-new")
            .unwrap()
            .read_at,
        Some(stamped),
        "re-mark must preserve the original read_at"
    );

    // Unknown ids are ignored, not an error, and change nothing.
    let unread = notes
        .mark_read(&alpha, "ada", Some(&["does-not-exist".to_string()]))
        .await
        .unwrap();
    assert_eq!(unread, 1);

    // `None` marks the whole company read for that person.
    let unread = notes.mark_read(&alpha, "ada", None).await.unwrap();
    assert_eq!(unread, 0);
    let ada = notes.list(&alpha, "ada").await.unwrap();
    assert!(ada.iter().all(|v| v.read_at.is_some()));
    let grace = notes.list(&alpha, "grace").await.unwrap();
    assert!(
        grace.iter().all(|v| v.read_at.is_none()),
        "Ada marking all read must not touch Grace"
    );

    // Ties in created_at break by id descending — a stable order the trait
    // documents, not each backend's insertion order. These two arrive after
    // Ada's `None` mark, so they are unread for her.
    notes
        .append(&alpha, &note("id-a", 300, SubjectKind::Run, "run-1"))
        .await
        .unwrap();
    notes
        .append(&alpha, &note("id-b", 300, SubjectKind::Workflow, "wf-1"))
        .await
        .unwrap();
    let ada = notes.list(&alpha, "ada").await.unwrap();
    assert_eq!(ada[0].notification.id, "id-b");
    assert_eq!(ada[1].notification.id, "id-a");

    // Per company: beta starts empty and stays independent of alpha.
    assert!(notes.list(&beta, "ada").await.unwrap().is_empty());
    notes
        .append(&beta, &note("b-1", 100, SubjectKind::Task, "task-9"))
        .await
        .unwrap();
    assert_eq!(notes.list(&beta, "ada").await.unwrap().len(), 1);
    assert_eq!(
        notes.list(&alpha, "ada").await.unwrap().len(),
        4,
        "beta's write must not change alpha"
    );
}

pub async fn assert_skill_state_store(skills: Arc<dyn SkillStateStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let state = |slug: &str, enabled: bool, source: SkillSource| SkillState {
        slug: slug.to_string(),
        enabled,
        source,
        custom_doc: None,
    };

    skills
        .set(&alpha, &state("web-research", true, SkillSource::Registry))
        .await
        .unwrap();
    skills
        .set(&beta, &state("leak", true, SkillSource::Custom))
        .await
        .unwrap();

    // Isolation.
    assert_eq!(skills.list(&beta).await.unwrap().len(), 1);

    // Upsert replaces by slug (a disable override).
    skills
        .set(&alpha, &state("web-research", false, SkillSource::Registry))
        .await
        .unwrap();
    let list = skills.list(&alpha).await.unwrap();
    assert_eq!(list.len(), 1);
    assert!(!list[0].enabled);

    // Custom doc round-trips.
    skills
        .set(
            &alpha,
            &SkillState {
                slug: "my-skill".to_string(),
                enabled: true,
                source: SkillSource::Custom,
                custom_doc: Some("---\nname: Mine\n---\nbody".to_string()),
            },
        )
        .await
        .unwrap();
    let custom = skills
        .list(&alpha)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.slug == "my-skill")
        .unwrap();
    assert!(custom.custom_doc.unwrap().contains("Mine"));

    // Remove.
    assert!(skills.remove(&alpha, "web-research").await.unwrap());
    assert!(!skills.remove(&alpha, "web-research").await.unwrap());
    assert_eq!(skills.list(&alpha).await.unwrap().len(), 1);
}

/// Asserts the [`WorkspaceStore`] contract: isolation, create/read/write,
/// rename+move (with cycle rejection), recursive delete, the seeding gate, and
/// the authorship stamps (issue #326).
pub async fn assert_workspace_store(ws: Arc<dyn WorkspaceStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let agent = || WorkspaceOrigin::Agent {
        id: "ceo".to_string(),
    };
    let node = |id: &str, name: &str, kind: NodeKind, parent: Option<&str>| WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind,
        parent_id: parent.map(str::to_string),
        updated_at_millis: now_millis(),
        created_by: WorkspaceOrigin::Agent {
            id: "ceo".to_string(),
        },
        updated_by: WorkspaceOrigin::Agent {
            id: "ceo".to_string(),
        },
        mime: None,
        size: None,
        sha256: None,
    };

    assert!(ws.is_empty(&alpha).await.unwrap());

    ws.create(&alpha, &node("root", "Brand", NodeKind::Folder, None), None)
        .await
        .unwrap();
    ws.create(
        &alpha,
        &node("note", "voice.md", NodeKind::File, Some("root")),
        Some("# Voice"),
    )
    .await
    .unwrap();
    ws.create(&beta, &node("b1", "Other", NodeKind::Folder, None), None)
        .await
        .unwrap();

    // Isolation + seeding gate.
    assert!(!ws.is_empty(&alpha).await.unwrap());
    assert_eq!(ws.tree(&alpha).await.unwrap().len(), 2);
    assert_eq!(ws.tree(&beta).await.unwrap().len(), 1);

    // Read a file's content; a folder yields empty.
    let (read_node, content) = ws.read(&alpha, "note").await.unwrap().unwrap();
    assert_eq!(read_node.name, "voice.md");
    assert_eq!(content, "# Voice");
    assert_eq!(ws.read(&alpha, "root").await.unwrap().unwrap().1, "");

    // Authorship round-trips through the backend's own storage (issue #326).
    // Every backend persists the node as opaque JSON, so this is the assertion
    // that a backend did not quietly drop a field it does not know about.
    assert_eq!(read_node.created_by, agent(), "created_by must round-trip");
    assert_eq!(read_node.updated_by, agent(), "updated_by must round-trip");

    // Overwrite content: `updated_by` follows the writer, `created_by` does not.
    let written = ws
        .write(&alpha, "note", "# Voice v2", WorkspaceOrigin::Operator)
        .await
        .unwrap();
    assert_eq!(
        written.updated_by,
        WorkspaceOrigin::Operator,
        "a write must restamp updated_by with its author"
    );
    assert_eq!(
        written.created_by,
        agent(),
        "a write must never rewrite created_by"
    );
    let (reread, body) = ws.read(&alpha, "note").await.unwrap().unwrap();
    assert_eq!(body, "# Voice v2");
    assert_eq!(
        (reread.created_by, reread.updated_by),
        (agent(), WorkspaceOrigin::Operator),
        "the write's stamps must be what the store persisted, not just what it returned"
    );

    // A second folder to move under.
    ws.create(
        &alpha,
        &node("root2", "Campaigns", NodeKind::Folder, None),
        None,
    )
    .await
    .unwrap();
    // Cycle rejection: cannot move a folder under its own descendant.
    ws.create(
        &alpha,
        &node("child", "Sub", NodeKind::Folder, Some("root")),
        None,
    )
    .await
    .unwrap();
    assert!(
        ws.rename_move(&alpha, "root", None, Some(Some("child")))
            .await
            .is_err(),
        "moving a folder under its descendant must be rejected"
    );

    // Rename + reparent the note under Campaigns.
    let moved = ws
        .rename_move(&alpha, "note", Some("voice-final.md"), Some(Some("root2")))
        .await
        .unwrap();
    assert_eq!(moved.name, "voice-final.md");
    assert_eq!(moved.parent_id.as_deref(), Some("root2"));
    // A rename/move leaves BOTH origins alone. This is the load-bearing half of
    // the #326 split: if a move restamped `updated_by`, an operator tidying an
    // agent's note into another folder would silently take credit for a body it
    // never touched.
    assert_eq!(
        (moved.created_by.clone(), moved.updated_by.clone()),
        (agent(), WorkspaceOrigin::Operator),
        "rename_move must not restamp authorship"
    );
    assert_eq!(
        ws.read(&alpha, "note").await.unwrap().unwrap().1,
        "# Voice v2",
        "content survives the move"
    );

    // Move the note back to the workspace root (`Some(None)` — an explicit
    // detach, distinct from `None` which would leave the parent unchanged).
    let to_root = ws
        .rename_move(&alpha, "note", None, Some(None))
        .await
        .unwrap();
    assert_eq!(to_root.parent_id, None, "explicit null moves to root");
    // A subsequent `None` leaves the (root) parent unchanged.
    let unchanged = ws.rename_move(&alpha, "note", None, None).await.unwrap();
    assert_eq!(
        unchanged.parent_id, None,
        "omitted parent leaves it at root"
    );

    // Recursive delete of a folder removes its descendants.
    assert!(ws.delete(&alpha, "root").await.unwrap());
    let tree = ws.tree(&alpha).await.unwrap();
    assert!(tree.iter().all(|n| n.id != "root" && n.id != "child"));
    assert!(!ws.delete(&alpha, "root").await.unwrap());
}

/// Collects a [`BlobStream`](crate::ports::workspace::BlobStream) into bytes.
///
/// Only the suite buffers: the port streams so a production download never has
/// to be resident, but an assertion about byte-exactness has to hold the whole
/// payload to make it.
async fn drain(stream: crate::ports::workspace::BlobStream) -> Vec<u8> {
    use futures::StreamExt;
    let mut out = Vec::new();
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("a blob chunk must not fail"));
    }
    out
}

/// Asserts the **binary** half of the [`WorkspaceStore`] contract (issue #553).
///
/// Kept separate from [`assert_workspace_store`] rather than folded into it,
/// because the two answer different questions: that one pins the tree every
/// backend has always had, this one pins the payload path added on top. A
/// backend can be wired into the first and not yet the second, and the split
/// makes that state visible instead of turning it into one large failure.
///
/// The suite deliberately includes a payload **larger than MongoDB's 16 MB BSON
/// document cap**. That is the case GridFS exists for, and it is the reason
/// this is a shared suite rather than a Mongo-only test: fs and sqlite run the
/// identical assertion, so "the big file round-trips" is a property of the
/// port, not a property of whichever backend somebody remembered to test.
pub async fn assert_workspace_binary_store(ws: Arc<dyn WorkspaceStore>) {
    let alpha = CompanyId::new("bin-alpha");
    let beta = CompanyId::new("bin-beta");
    let agent = || WorkspaceOrigin::Agent {
        id: "designer".to_string(),
    };
    let node = |id: &str, name: &str, mime: Option<&str>, parent: Option<&str>| WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind: NodeKind::File,
        parent_id: parent.map(str::to_string),
        updated_at_millis: now_millis(),
        created_by: agent(),
        updated_by: agent(),
        mime: mime.map(str::to_string),
        // Deliberately wrong, and deliberately not `None`: the store must
        // compute these from the bytes and must not carry a caller's claim
        // about them through to storage.
        size: Some(999_999),
        sha256: Some("not-a-real-digest".to_string()),
    };

    // A payload that is emphatically not text: a lone continuation byte, an
    // interior NUL, and a byte sequence no UTF-8 decoder accepts. A backend
    // that round-trips through `String` anywhere fails here rather than
    // silently replacing bytes with U+FFFD.
    let png: Vec<u8> = vec![
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff, 0xfe, 0xc0, 0x80, 0x01,
    ];

    ws.create(&alpha, &folder_node("shots", "Shots"), None)
        .await
        .unwrap();
    let stamped = ws
        .create_binary(
            &alpha,
            &node("img", "hero.png", Some("image/png"), Some("shots")),
            &png,
        )
        .await
        .unwrap();

    // -- The create RETURNS the stamped node (issue #668) ------------------
    //
    // Asserted with populated values, never `None`: a `None` here would pass
    // against a backend that dropped the field entirely, which is the one thing
    // this is meant to catch. The digest is the reason the signature returns a
    // node at all — a caller that records it must be unable to obtain it from
    // anywhere but the store.
    let (expected_size, expected_sha) = crate::ports::workspace::blob_metadata(&png);
    assert_eq!(
        stamped.sha256.as_deref(),
        Some(expected_sha.as_str()),
        "create_binary must hand back the digest it computed, not an empty field"
    );
    assert_eq!(
        stamped.size,
        Some(expected_size),
        "and the size it computed with it"
    );
    assert_eq!(
        stamped.id, "img",
        "the returned node is the one just created"
    );
    assert_eq!(stamped.mime.as_deref(), Some("image/png"));

    // -- Metadata is computed, not accepted -------------------------------
    let (meta, stream) = ws.read_bytes(&alpha, "img").await.unwrap().unwrap();
    assert_eq!(meta.mime.as_deref(), Some("image/png"));
    assert_eq!(
        meta.size,
        Some(png.len() as u64),
        "size must come from the bytes, not from the caller's claim"
    );
    assert_eq!(
        meta.sha256.as_deref(),
        Some(expected_sha.as_str()),
        "sha256 must be computed from the stored bytes, not trusted from the caller"
    );
    assert_eq!(
        drain(stream).await,
        png,
        "the payload must round-trip byte-exactly"
    );

    // -- Authorship survives the binary path ------------------------------
    assert_eq!(meta.created_by, agent());
    assert_eq!(meta.updated_by, agent());

    // -- The tree carries the metadata ------------------------------------
    let in_tree = ws
        .tree(&alpha)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.id == "img")
        .expect("the binary node is in the tree");
    assert_eq!(in_tree.mime.as_deref(), Some("image/png"));
    assert_eq!(in_tree.size, Some(png.len() as u64));
    assert!(in_tree.is_binary());

    // -- A text read of a binary node is empty, never bytes-as-text -------
    let (text_node, body) = ws.read(&alpha, "img").await.unwrap().unwrap();
    assert!(body.is_empty(), "a binary node reads as an empty body");
    assert!(text_node.is_binary());

    // -- A text write over a payload is refused ---------------------------
    let refused = ws
        .write(&alpha, "img", "# not an image", WorkspaceOrigin::Operator)
        .await
        .expect_err("writing text over a payload must be refused");
    assert!(
        refused.to_string().contains("image/png"),
        "the refusal must name what the node actually holds, got: {refused}"
    );
    // …and the refusal changed nothing.
    let (still, stream) = ws.read_bytes(&alpha, "img").await.unwrap().unwrap();
    assert_eq!(still.sha256.as_deref(), Some(expected_sha.as_str()));
    assert_eq!(drain(stream).await, png);

    // -- `read_bytes` of a prose note, and of a folder, is None -----------
    ws.create(
        &alpha,
        &WorkspaceNode {
            mime: None,
            size: None,
            sha256: None,
            ..node("note", "brief.md", None, None)
        },
        Some("# Brief"),
    )
    .await
    .unwrap();
    assert!(
        ws.read_bytes(&alpha, "note").await.unwrap().is_none(),
        "a prose note has no payload"
    );
    assert!(
        ws.read_bytes(&alpha, "shots").await.unwrap().is_none(),
        "a folder has no payload"
    );
    assert!(ws.read_bytes(&alpha, "nope").await.unwrap().is_none());

    // -- Replacing a payload keeps the node and restamps it ---------------
    let replaced = ws
        .write_binary(&alpha, "img", &[0x00, 0x01, 0x02], Some("image/jpeg"), {
            WorkspaceOrigin::Operator
        })
        .await
        .unwrap();
    assert_eq!(replaced.mime.as_deref(), Some("image/jpeg"));
    assert_eq!(replaced.size, Some(3));
    assert_eq!(
        replaced.updated_by,
        WorkspaceOrigin::Operator,
        "a payload replacement restamps updated_by like a text write"
    );
    assert_eq!(
        replaced.created_by,
        agent(),
        "and never rewrites created_by"
    );
    let (_, stream) = ws.read_bytes(&alpha, "img").await.unwrap().unwrap();
    assert_eq!(
        drain(stream).await,
        vec![0x00, 0x01, 0x02],
        "the old payload must be gone, not merely shadowed"
    );

    // Writing bytes over a prose note is the mirror refusal of the text case.
    assert!(
        ws.write_binary(&alpha, "note", &[1, 2], Some("image/png"), agent())
            .await
            .is_err(),
        "a prose note must not be convertible into a payload by a write"
    );

    // -- Rename/move leaves the payload intact ----------------------------
    ws.create(&alpha, &folder_node("archive", "Archive"), None)
        .await
        .unwrap();
    let moved = ws
        .rename_move(&alpha, "img", Some("hero-final.jpg"), Some(Some("archive")))
        .await
        .unwrap();
    assert_eq!(moved.name, "hero-final.jpg");
    assert_eq!(
        moved.mime.as_deref(),
        Some("image/jpeg"),
        "a move must not disturb the payload metadata"
    );
    let (after_move, stream) = ws.read_bytes(&alpha, "img").await.unwrap().unwrap();
    assert_eq!(after_move.size, Some(3));
    assert_eq!(
        drain(stream).await,
        vec![0x00, 0x01, 0x02],
        "the bytes must survive a rename and reparent"
    );

    // -- Cross-company isolation, including through the blob store --------
    ws.create_binary(
        &beta,
        &node("img", "other.png", Some("image/png"), None),
        b"beta-only-bytes",
    )
    .await
    .unwrap();
    let (_, stream) = ws.read_bytes(&beta, "img").await.unwrap().unwrap();
    assert_eq!(
        drain(stream).await,
        b"beta-only-bytes".to_vec(),
        "the same node id in another company must resolve to that company's payload"
    );
    let (_, stream) = ws.read_bytes(&alpha, "img").await.unwrap().unwrap();
    assert_eq!(
        drain(stream).await,
        vec![0x00, 0x01, 0x02],
        "and must not have been overwritten by it"
    );

    // -- Delete removes the payload, not just the node --------------------
    assert!(ws.delete(&beta, "img").await.unwrap());
    assert!(
        ws.read_bytes(&beta, "img").await.unwrap().is_none(),
        "deleting a node must delete its payload"
    );
    // A folder delete takes its binary descendants' payloads with it.
    assert!(ws.delete(&alpha, "archive").await.unwrap());
    assert!(
        ws.read_bytes(&alpha, "img").await.unwrap().is_none(),
        "a recursive delete must remove descendants' payloads too"
    );

    // -- Past the 16 MB BSON document cap, under the per-write cap --------
    //
    // 17 MiB does double duty. It is past MongoDB's 16 MB BSON document limit,
    // which is the case GridFS exists for — and it is comfortably under
    // `DEFAULT_MAX_BLOB_BYTES` (64 MiB), so it is also the proof that a large
    // but legitimate payload still round-trips on **all three** backends rather
    // than being caught by the cap. The boundary either side of the cap itself
    // is asserted on the quota decorator, where the comparison lives.
    //
    // Patterned rather than zeroed so a backend that silently truncates or pads
    // is caught by the digest instead of matching by luck.
    let big: Vec<u8> = (0..17 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let (big_size, big_sha) = crate::ports::workspace::blob_metadata(&big);
    ws.create_binary(
        &alpha,
        &node("big", "render.mp4", Some("video/mp4"), None),
        &big,
    )
    .await
    .unwrap();
    let (big_meta, stream) = ws.read_bytes(&alpha, "big").await.unwrap().unwrap();
    assert_eq!(big_meta.size, Some(big_size));
    assert_eq!(big_meta.sha256.as_deref(), Some(big_sha.as_str()));
    let got = drain(stream).await;
    assert_eq!(
        got.len(),
        big.len(),
        "a payload past the BSON document cap must round-trip whole"
    );
    assert_eq!(
        crate::ports::workspace::blob_metadata(&got).1,
        big_sha,
        "…and byte-exactly"
    );

    // -- Atomic staged-file swap -----------------------------------------
    let old = WorkspaceNode {
        mime: None,
        size: None,
        sha256: None,
        ..node("swap-old", "report.md", None, None)
    };
    ws.create(&alpha, &old, Some("# old")).await.unwrap();
    ws.create_binary(
        &alpha,
        &node(
            "swap-new",
            "report.md.publishing-test",
            Some("application/pdf"),
            None,
        ),
        b"new-payload",
    )
    .await
    .unwrap();
    let promoted = ws
        .swap_files(&alpha, Some("swap-old"), "swap-new", "report.md")
        .await
        .unwrap()
        .expect("the expected node is still current");
    assert_eq!(promoted.id, "swap-new");
    assert_eq!(promoted.name, "report.md");
    assert!(ws.read(&alpha, "swap-old").await.unwrap().is_none());
    let (_, stream) = ws.read_bytes(&alpha, "swap-new").await.unwrap().unwrap();
    assert_eq!(drain(stream).await, b"new-payload".to_vec());

    // A stale compare-and-swap loses and consumes its private stage, payload
    // included. This is what prevents two concurrent publishers from leaving
    // either a duplicate final path or quota-charging garbage behind.
    ws.create_binary(
        &alpha,
        &node(
            "swap-loser",
            "report.md.publishing-loser",
            Some("application/pdf"),
            None,
        ),
        b"loser",
    )
    .await
    .unwrap();
    assert!(
        ws.swap_files(&alpha, Some("already-gone"), "swap-loser", "report.md")
            .await
            .unwrap()
            .is_none()
    );
    assert!(ws.read(&alpha, "swap-loser").await.unwrap().is_none());
    assert!(ws.read_bytes(&alpha, "swap-loser").await.unwrap().is_none());

    // -- Conditional first publish (issue #697) ---------------------------
    //
    // `None` installs only while the name is unoccupied. `report.md` is
    // occupied at this point — the swap above promoted `swap-new` onto it — so
    // this must LOSE rather than overwrite it.
    //
    // This case is the whole reason the parameter is an `Option` rather than
    // two methods: `Some(id)` and `None` are one type apart and mean opposite
    // things, and a caller that passes `None` meaning "replace whatever is
    // there" compiles. Nothing but this assertion stops that mistake from
    // silently reintroducing the duplicate-path race it was meant to fix.
    ws.create_binary(
        &alpha,
        &node(
            "create-onto-occupied",
            "report.md.publishing-occupied",
            Some("application/pdf"),
            None,
        ),
        b"must-not-land",
    )
    .await
    .unwrap();
    assert!(
        ws.swap_files(&alpha, None, "create-onto-occupied", "report.md")
            .await
            .unwrap()
            .is_none(),
        "`None` asserts the name is free; it must not overwrite an occupant"
    );
    let survivor = ws.read(&alpha, "swap-new").await.unwrap();
    assert!(
        survivor.is_some(),
        "the node that held the path must still hold it"
    );
    let (_, stream) = ws.read_bytes(&alpha, "swap-new").await.unwrap().unwrap();
    assert_eq!(
        drain(stream).await,
        b"new-payload".to_vec(),
        "and its payload must be untouched"
    );
    // The loser consumes its own stage on this arm too, payload included.
    assert!(
        ws.read(&alpha, "create-onto-occupied")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        ws.read_bytes(&alpha, "create-onto-occupied")
            .await
            .unwrap()
            .is_none()
    );

    // And the arm that succeeds: a name nothing holds is installed, keeping
    // the staged node's id and taking the final name.
    ws.create_binary(
        &alpha,
        &node(
            "create-fresh",
            "fresh.md.publishing-test",
            Some("application/pdf"),
            None,
        ),
        b"fresh-payload",
    )
    .await
    .unwrap();
    let created = ws
        .swap_files(&alpha, None, "create-fresh", "fresh.md")
        .await
        .unwrap()
        .expect("the name was free");
    assert_eq!(created.id, "create-fresh");
    assert_eq!(created.name, "fresh.md");
    let (_, stream) = ws
        .read_bytes(&alpha, "create-fresh")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(drain(stream).await, b"fresh-payload".to_vec());
    // Exactly one node answers to the new name.
    let tree = ws.tree(&alpha).await.unwrap();
    assert_eq!(
        tree.iter().filter(|n| n.name == "fresh.md").count(),
        1,
        "a first publish must leave exactly one node at the path: {tree:?}"
    );

    // -- A binary node must carry a mime ----------------------------------
    assert!(
        ws.create_binary(&alpha, &node("nomime", "x.bin", None, None), b"x")
            .await
            .is_err(),
        "a binary node without a mime type is refused"
    );
    // …and must be a file.
    assert!(
        ws.create_binary(
            &alpha,
            &WorkspaceNode {
                kind: NodeKind::Folder,
                ..node("asfolder", "Nope", Some("image/png"), None)
            },
            b"x"
        )
        .await
        .is_err(),
        "a folder cannot hold a payload"
    );
}

/// Every backend decides a folder claim the same way — including under
/// contention (issue #759).
///
/// The contention case at the end is the one that matters. A naive
/// read-then-create passes every sequential assertion above it and fails only
/// there, which is precisely the shape of the defect: each backend's answer was
/// correct about the instant it looked and wrong by the time it wrote. It is
/// also what proves the MongoDB partial unique index is actually deciding, since
/// nothing else on that backend can.
pub async fn assert_workspace_folder_claims(ws: Arc<dyn WorkspaceStore>) {
    use crate::ports::workspace::{
        FolderClaim, folder_claim_ambiguous_refusal, folder_claim_file_refusal,
    };

    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let cmo = WorkspaceOrigin::Agent {
        id: "cmo".to_string(),
    };
    let cto = WorkspaceOrigin::Agent {
        id: "cto".to_string(),
    };

    // -- Created when the name is free ------------------------------------
    let claim = ws
        .adopt_or_create_folder(&alpha, None, "Agents", cmo.clone())
        .await
        .expect("a free root name is claimable");
    assert!(claim.was_created(), "nothing was there to adopt");
    let root = claim.node().clone();
    assert_eq!(root.name, "Agents");
    assert_eq!(root.kind, NodeKind::Folder);
    assert_eq!(root.parent_id, None);
    assert_eq!(
        root.created_by, cmo,
        "the creating caller's origin is stamped"
    );
    assert_eq!(root.updated_by, cmo);
    assert!(
        ws.tree(&alpha)
            .await
            .unwrap()
            .iter()
            .any(|n| n.id == root.id),
        "a created folder is in the tree"
    );

    // -- Adopted, idempotently, keeping the original authorship -----------
    let again = ws
        .adopt_or_create_folder(&alpha, None, "Agents", cto.clone())
        .await
        .expect("an existing folder is adopted, not refused");
    assert!(!again.was_created(), "the folder was already there");
    assert_eq!(again.node().id, root.id, "adoption returns the same folder");
    assert_eq!(
        again.node().created_by,
        cmo,
        "adoption must not rewrite whose folder it is"
    );
    assert_eq!(
        ws.tree(&alpha)
            .await
            .unwrap()
            .iter()
            .filter(|n| n.parent_id.is_none() && n.name == "Agents")
            .count(),
        1,
        "and it must not have minted a rival"
    );

    // -- Nested under a parent, and only under that parent -----------------
    let nested = ws
        .adopt_or_create_folder(&alpha, Some(&root.id), "cmo", cmo.clone())
        .await
        .expect("a folder under a folder");
    assert!(nested.was_created());
    assert_eq!(nested.node().parent_id.as_deref(), Some(root.id.as_str()));
    // The same name at the root is a different path and must be free there.
    let sibling_at_root = ws
        .adopt_or_create_folder(&alpha, None, "cmo", cmo.clone())
        .await
        .expect("the root is a different parent");
    assert!(sibling_at_root.was_created());
    assert_ne!(sibling_at_root.node().id, nested.node().id);

    // -- A file holding the name is refused, identically on every backend --
    let note = WorkspaceNode {
        id: "claim-note".to_string(),
        name: "notes.md".to_string(),
        kind: NodeKind::File,
        parent_id: None,
        updated_at_millis: now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
    };
    ws.create(&alpha, &note, Some("body")).await.unwrap();
    let refused = ws
        .adopt_or_create_folder(&alpha, None, "notes.md", cmo.clone())
        .await
        .expect_err("a note cannot be adopted as a folder");
    assert!(
        refused
            .to_string()
            .ends_with(&folder_claim_file_refusal("notes.md")),
        "the refusal must be the shared one, or it drifts between backends: {refused}"
    );

    // -- A missing or non-folder parent is refused ------------------------
    assert!(
        ws.adopt_or_create_folder(&alpha, Some("no-such-parent"), "x", cmo.clone())
            .await
            .is_err(),
        "a claim under a parent that does not exist is refused"
    );
    assert!(
        ws.adopt_or_create_folder(&alpha, Some("claim-note"), "x", cmo.clone())
            .await
            .is_err(),
        "a claim under a *file* is refused"
    );

    // -- Pre-existing ambiguity stays fail-closed -------------------------
    //
    // Written through `create` rather than through the primitive, because the
    // primitive is exactly what makes this state unreachable from now on. It is
    // still reachable from *history*: a tenant that lost this race before the
    // guard existed carries it, and the answer must be a refusal rather than a
    // third node piled on top. Ids are supplied, so no backend has to accept a
    // name it would refuse — only a `create` that skips the sibling check, which
    // sqlite and mongodb both do.
    for id in ["dup-a", "dup-b"] {
        let dup = WorkspaceNode {
            id: id.to_string(),
            name: "Legacy".to_string(),
            kind: NodeKind::Folder,
            parent_id: Some(root.id.clone()),
            ..note.clone()
        };
        // `fs` refuses the second by design (issue #666); it has never been able
        // to represent this state, so it simply has nothing to fail closed on.
        if ws.create(&alpha, &dup, None).await.is_err() {
            break;
        }
    }
    let legacy: Vec<WorkspaceNode> = ws
        .tree(&alpha)
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.name == "Legacy")
        .collect();
    if legacy.len() > 1 {
        let refused = ws
            .adopt_or_create_folder(&alpha, Some(&root.id), "Legacy", cmo.clone())
            .await
            .expect_err("an ambiguous path must stay refused");
        assert!(
            refused
                .to_string()
                .ends_with(&folder_claim_ambiguous_refusal("Legacy", legacy.len())),
            "the ambiguity refusal must be the shared one: {refused}"
        );
    }

    // -- Companies do not see each other's claims -------------------------
    let elsewhere = ws
        .adopt_or_create_folder(&beta, None, "Agents", cmo.clone())
        .await
        .expect("another company's identical path is free");
    assert!(
        elsewhere.was_created(),
        "company beta must not adopt company alpha's folder"
    );
    assert_ne!(elsewhere.node().id, root.id);

    // -- Eight-way contention on one path ---------------------------------
    //
    // The assertion the sequential cases cannot make. All eight callers must
    // succeed, all eight must be holding the SAME folder, and exactly one may
    // report having created it — that last count is what a duplicated folder
    // would break even where the ids happened to agree.
    let contested = Arc::new(root.id.clone());
    let mut racers = Vec::new();
    for i in 0..8 {
        let ws = ws.clone();
        let alpha = alpha.clone();
        let parent = contested.clone();
        let origin = WorkspaceOrigin::Agent {
            id: format!("racer-{i}"),
        };
        racers.push(tokio::spawn(async move {
            ws.adopt_or_create_folder(&alpha, Some(&parent), "task-42", origin)
                .await
        }));
    }
    let mut ids = Vec::new();
    let mut created = 0usize;
    for racer in racers {
        let claim = racer
            .await
            .expect("the claim task must not panic")
            .expect("every caller must come away with the folder, winner or not");
        if matches!(claim, FolderClaim::Created(_)) {
            created += 1;
        }
        ids.push(claim.into_node().id);
    }
    assert_eq!(created, 1, "exactly one caller may mint the folder");
    assert!(
        ids.windows(2).all(|pair| pair[0] == pair[1]),
        "every caller must hold the same folder: {ids:?}"
    );
    let tree = ws.tree(&alpha).await.unwrap();
    assert_eq!(
        tree.iter()
            .filter(|n| n.parent_id.as_deref() == Some(root.id.as_str()) && n.name == "task-42")
            .count(),
        1,
        "one folder under one name, or every later publish beneath it is refused: {tree:?}"
    );

    // …and it stays claimable afterwards, which is the no-permanent-outage half.
    let after = ws
        .adopt_or_create_folder(&alpha, Some(&root.id), "task-42", cmo)
        .await
        .expect("the contested path must still resolve");
    assert!(!after.was_created());
    assert_eq!(&after.node().id, &ids[0]);
}

/// Asserts that a reader concurrent with a writer on the SAME node never errors
/// and never observes a partial body (issue #887).
///
/// This is a statement about
/// [`WorkspaceStore::read`](crate::ports::workspace::WorkspaceStore::read), so
/// it is stated here rather than as an `fs` test: every backend a hosted tenant
/// can run has to answer for it. sqlite and mongodb already do — a row update
/// and a document replace are atomic — which is exactly why the guarantee
/// belongs to the port and not to whichever backend happened to have it.
///
/// The `fs` backend did not. It wrote node content with a bare
/// `tokio::fs::write`, whose `O_TRUNC` leaves the file short for the whole of
/// the write, while `read` takes no lock. A reader landing in that window sees a
/// prefix, and the two ways that surfaces are not equally visible:
///
/// * the cut lands **mid-codepoint** → `read_to_string` fails with
///   `InvalidData`, which at least produces a red step; and
/// * the cut lands **on** a codepoint boundary → the read **succeeds** with half
///   a document, the agent grounds an answer in it, and nothing anywhere says
///   so.
///
/// Sibling-name uniqueness for files, which every backend must enforce **in the
/// store** (issue #894).
///
/// SQLite had no equivalent of fs's `reject_path_collision` or MongoDB's partial
/// unique index: `workspace_nodes` is `PRIMARY KEY (company_id, id)` and nothing
/// else, so `create` checked only that the *id* was fresh. Two nodes could share
/// `(company_id, parent_id, name)`, and from then on `render_path` yields one
/// path for two nodes — `read` answers `Ambiguous` while `list` still shows
/// both, and one duplicated ancestor folder poisons every descendant path.
///
/// This case is the contract, not the race: it runs sequentially, so it holds
/// every backend to the *rule* without depending on scheduling. The race itself
/// is decided by machinery only each backend has — see the SQLite store's
/// `two_stores_racing_one_name_have_one_winner`, which is where the guard
/// actually earns its transaction.
///
/// **Files only, deliberately.** Folder-vs-folder is `adopt_or_create_folder`'s
/// to adopt rather than refuse (issue #759), and file-vs-folder is left
/// unasserted because the backends genuinely disagree — see the note at the end
/// of the body. Asserting more here would make this a new tree rule rather than
/// a race fix, and would fail one backend or the other whichever way it went.
pub async fn assert_workspace_sibling_names(ws: Arc<dyn WorkspaceStore>) {
    use crate::ports::workspace::WorkspaceOrigin;

    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let agent = || WorkspaceOrigin::Agent {
        id: "ceo".to_string(),
    };
    let file = |id: &str, name: &str, parent: Option<&str>| WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind: NodeKind::File,
        parent_id: parent.map(str::to_string),
        created_by: agent(),
        updated_by: agent(),
        updated_at_millis: now_millis(),
        mime: None,
        size: None,
        sha256: None,
    };

    // A folder to hold the contended name, plus the root as a second scope.
    let folder = WorkspaceNode {
        kind: NodeKind::Folder,
        ..file("dup-folder", "Reports", None)
    };
    ws.create(&alpha, &folder, None)
        .await
        .expect("a folder at a free root name");

    ws.create(
        &alpha,
        &file("dup-a", "report.md", Some("dup-folder")),
        Some("A"),
    )
    .await
    .expect("the first file claims the name");

    // -- The rule ---------------------------------------------------------
    let err = ws
        .create(
            &alpha,
            &file("dup-b", "report.md", Some("dup-folder")),
            Some("B"),
        )
        .await
        .expect_err("a second file at one path must be refused by the store");
    assert!(
        matches!(err, crate::error::OpenCompanyError::Conflict(_)),
        "a taken sibling name is a Conflict, not a storage fault: {err:?}"
    );

    // -- And it left nothing behind ---------------------------------------
    let tree = ws.tree(&alpha).await.expect("tree reads");
    let named: Vec<&WorkspaceNode> = tree
        .iter()
        .filter(|n| n.name == "report.md" && n.parent_id.as_deref() == Some("dup-folder"))
        .collect();
    assert_eq!(
        named.len(),
        1,
        "exactly one node may hold the path; the loser must not be stored: {named:?}"
    );
    assert_eq!(named[0].id, "dup-a", "the first writer keeps the name");
    let (_, winner_body) = ws
        .read(&alpha, "dup-a")
        .await
        .expect("winner reads")
        .expect("the winner is still there");
    assert_eq!(
        winner_body, "A",
        "the winner's payload is untouched by the refusal"
    );

    // -- Scope: the same name under a different parent is a different path -
    ws.create(&alpha, &file("dup-root", "report.md", None), None)
        .await
        .expect("the root is a different folder, so the name is free there");

    // -- Scope: and a different company shares nothing ---------------------
    let beta_folder = WorkspaceNode {
        kind: NodeKind::Folder,
        ..file("dup-folder", "Reports", None)
    };
    ws.create(&beta, &beta_folder, None)
        .await
        .expect("beta's own folder");
    ws.create(&beta, &file("dup-a", "report.md", Some("dup-folder")), None)
        .await
        .expect("another company's tree is not consulted");

    // A folder taking a file's name is deliberately NOT asserted either way.
    // The backends genuinely disagree and always have: fs's
    // `reject_path_collision` compares the rendered path and so refuses it,
    // while MongoDB keys files and folders under separate partial indexes and
    // so permits it. Pinning either answer here would force one of them to
    // change behaviour — loosening the strictest backend, or widening a race
    // fix into a new tree rule. The suite states the rule all three must keep
    // and stays silent on the rest.
}

/// A retry only ever addresses the first. So the body is deliberately built from
/// multi-byte characters and checked for **equality with a whole revision**
/// rather than for decodability: length and content, not just "no error".
pub async fn assert_workspace_read_never_tears(ws: Arc<dyn WorkspaceStore>) {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// How many times the note is rewritten end to end.
    const ROUNDS: usize = 60;
    /// Concurrent readers. More than one, because a single reader spends much of
    /// its time not inside the window.
    const READERS: usize = 4;
    /// Repeats of the 8-byte unit below, giving a ~512 KiB body — large enough
    /// that the write is not one instantaneous syscall.
    const UNITS: usize = 64 * 1024;

    let company = CompanyId::new("alpha");
    // Every character is multi-byte, so a cut at an odd offset splits one. That
    // is the visible half of the failure; the silent half is a cut that does
    // not, which the equality check below is what catches.
    let whole_a: String = "αβγδ".repeat(UNITS);
    let whole_b: String = "εζηθ".repeat(UNITS);
    assert_eq!(
        whole_a.len(),
        whole_b.len(),
        "the two revisions must be the same size, so a short read cannot be \
         mistaken for the other revision"
    );

    let node = WorkspaceNode {
        id: "torn-note".to_string(),
        name: "Torn.md".to_string(),
        kind: NodeKind::File,
        parent_id: None,
        updated_at_millis: now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
    };
    ws.create(&company, &node, Some(&whole_a))
        .await
        .expect("seed the note");

    let done = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let ws = Arc::clone(&ws);
            let company = company.clone();
            let done = Arc::clone(&done);
            let (a, b) = (whole_a.clone(), whole_b.clone());
            tokio::spawn(async move {
                let mut observed = 0usize;
                while !done.load(Ordering::Relaxed) {
                    let body = match ws.read(&company, "torn-note").await {
                        Ok(Some((_, body))) => body,
                        Ok(None) => {
                            return Err(
                                "the note vanished; nothing in this test deletes it".to_string()
                            );
                        }
                        Err(e) => {
                            return Err(format!(
                                "a read concurrent with a write FAILED ({e}). \
                                 `read` has no failure mode of its own here — the note exists \
                                 and is unchanged in size; this is the writer's truncation \
                                 window observed from the outside."
                            ));
                        }
                    };
                    if body != a && body != b {
                        return Err(format!(
                            "a read concurrent with a write observed a PARTIAL body: \
                             {seen} bytes, where either whole revision is {whole}. \
                             It decoded cleanly, so nothing failed and nothing was logged — \
                             an agent would have grounded its answer in this.",
                            seen = body.len(),
                            whole = a.len(),
                        ));
                    }
                    observed += 1;
                    // Yield so a current-thread runtime interleaves the writer.
                    tokio::task::yield_now().await;
                }
                Ok(observed)
            })
        })
        .collect();

    for round in 0..ROUNDS {
        let body = if round % 2 == 0 { &whole_b } else { &whole_a };
        ws.write(&company, "torn-note", body, WorkspaceOrigin::Operator)
            .await
            .expect("the writer itself must not fail");
    }
    done.store(true, Ordering::Relaxed);

    let mut total = 0usize;
    for reader in readers {
        match reader.await.expect("a reader task panicked") {
            Ok(observed) => total += observed,
            Err(why) => panic!("{why}"),
        }
    }
    assert!(
        total > 0,
        "no read ran while the note was being rewritten, so this case proved nothing"
    );

    // And the note is one of the two whole revisions once everything settles.
    let (_, final_body) = ws
        .read(&company, "torn-note")
        .await
        .expect("the settled read")
        .expect("the note is still there");
    assert!(final_body == whole_a || final_body == whole_b);
}

/// A folder node for the binary suite.
fn folder_node(id: &str, name: &str) -> WorkspaceNode {
    WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind: NodeKind::Folder,
        parent_id: None,
        updated_at_millis: now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
    }
}

// ---------------------------------------------------------------------------
// RunStore (issue #242)
// ---------------------------------------------------------------------------
//
// Imports for this suite are function-local rather than added to the module
// header: the header is being edited concurrently on another branch, and a
// `use` inside the function keeps this suite a pure append.

/// Asserts the [`RunStore`](crate::ports::runs::RunStore) contract: per-company
/// isolation, per-task attempt ordinals, transition legality, the step trace,
/// and the list filters.
pub async fn assert_run_store(runs: Arc<dyn crate::ports::runs::RunStore>) {
    use crate::ports::runs::{NewRun, RunFilter, RunOutcome, RunStatus, RunStepRecord};
    use crate::ports::types::{TokenUsage, TurnStep, TurnStepKind, TurnStepStatus};

    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let spec = |id: &str, task: &str| NewRun::for_task(id, task, "ceo");

    // -- create: a fresh run is Pending and nothing else ---------------------

    let first = runs.create_run(&alpha, spec("r1", "card")).await.unwrap();
    assert_eq!(first.status, RunStatus::Pending);
    assert_eq!(first.attempt, 1, "the first attempt at a card is 1-based");
    assert_eq!(first.company, alpha);
    assert_eq!(first.task_id.as_deref(), Some("card"));
    assert_eq!(first.chat_id, None, "a dispatch names no conversation");
    assert_eq!(first.agent_id, "ceo");
    assert_eq!(first.trigger_event_seq, None);
    assert_eq!(first.started_at_millis, None);
    assert_eq!(first.finished_at_millis, None);
    assert_eq!(first.error, None);
    assert_eq!(first.step_count, 0);
    assert!(first.created_at_millis > 0);

    // Read-back is byte-identical (the export-totality precondition).
    assert_eq!(runs.get_run(&alpha, "r1").await.unwrap(), Some(first));

    // -- attempt ordinals are per card, not per company ----------------------

    let second = runs.create_run(&alpha, spec("r2", "card")).await.unwrap();
    assert_eq!(second.attempt, 2);
    let third = runs.create_run(&alpha, spec("r3", "card")).await.unwrap();
    assert_eq!(third.attempt, 3);
    let other_card = runs.create_run(&alpha, spec("r4", "other")).await.unwrap();
    assert_eq!(
        other_card.attempt, 1,
        "a different card starts its own attempt count"
    );

    // A repeated id is a conflict, never a silent overwrite of a live attempt.
    assert!(
        runs.create_run(&alpha, spec("r1", "card")).await.is_err(),
        "creating a run with an existing id must fail"
    );

    // -- company isolation ---------------------------------------------------

    let beta_run = runs.create_run(&beta, spec("b1", "card")).await.unwrap();
    assert_eq!(
        beta_run.attempt, 1,
        "attempt ordinals do not leak across companies"
    );
    assert!(runs.get_run(&beta, "r1").await.unwrap().is_none());
    assert!(runs.get_run(&alpha, "b1").await.unwrap().is_none());
    let beta_all = runs.list_runs(&beta, &RunFilter::default()).await.unwrap();
    assert_eq!(beta_all.len(), 1);
    assert_eq!(beta_all[0].id, "b1");

    // -- begin_run: Pending → Running ---------------------------------------

    let begun = runs
        .begin_run(&alpha, "r1", EventSeq::new(7))
        .await
        .unwrap();
    assert_eq!(begun.status, RunStatus::Running);
    assert_eq!(begun.trigger_event_seq, Some(EventSeq::new(7)));
    assert!(begun.started_at_millis.is_some());
    assert_eq!(begun.finished_at_millis, None);
    assert_eq!(runs.get_run(&alpha, "r1").await.unwrap(), Some(begun));

    // A second begin on a live run is a caller bug, not an idempotent no-op.
    assert!(
        runs.begin_run(&alpha, "r1", EventSeq::new(8))
            .await
            .is_err(),
        "Running → Running must be rejected"
    );

    // A transition against a run that does not exist is an error, not a
    // silently created row.
    assert!(
        runs.begin_run(&alpha, "nope", EventSeq::new(1))
            .await
            .is_err()
    );
    assert!(runs.get_run(&alpha, "nope").await.unwrap().is_none());

    // -- finish_run: cost, step count and terminality ------------------------

    let usage = TokenUsage {
        input: 120,
        output: 60,
        cached_input: 10,
        cost_usd: 0.5,
    };
    let settled = runs
        .finish_run(
            &alpha,
            "r1",
            RunOutcome {
                status: RunStatus::Succeeded,
                error: None,
                usage,
                step_count: 3,
            },
        )
        .await
        .unwrap();
    assert_eq!(settled.status, RunStatus::Succeeded);
    assert_eq!(settled.usage, usage);
    assert_eq!(settled.step_count, 3);
    assert!(
        settled.finished_at_millis.is_some(),
        "a terminal settle stamps the finish time"
    );
    assert_eq!(runs.get_run(&alpha, "r1").await.unwrap(), Some(settled));

    // Terminal is final: nothing moves out of it. A re-run is a NEW attempt.
    assert!(
        runs.finish_run(&alpha, "r1", RunOutcome::new(RunStatus::Failed))
            .await
            .is_err()
    );
    assert!(
        runs.begin_run(&alpha, "r1", EventSeq::new(9))
            .await
            .is_err()
    );
    assert_eq!(
        runs.get_run(&alpha, "r1").await.unwrap().unwrap().status,
        RunStatus::Succeeded,
        "a rejected transition leaves the row untouched"
    );

    // `finish_run` is how a run stops advancing — it can never start one.
    assert!(
        runs.finish_run(&alpha, "r2", RunOutcome::new(RunStatus::Running))
            .await
            .is_err()
    );
    assert!(
        runs.finish_run(&alpha, "r2", RunOutcome::new(RunStatus::Pending))
            .await
            .is_err()
    );

    // -- parked runs are not finished runs (epic #183 decisions 2 and 3) -----

    runs.begin_run(&alpha, "r2", EventSeq::new(10))
        .await
        .unwrap();
    let parked = runs
        .finish_run(&alpha, "r2", RunOutcome::new(RunStatus::WaitingApproval))
        .await
        .unwrap();
    assert_eq!(parked.status, RunStatus::WaitingApproval);
    assert_eq!(
        parked.finished_at_millis, None,
        "a parked run can still resume, so it carries no finish time"
    );

    // Re-enterable: #243 grants are single-use, so one attempt may stop for
    // review many times.
    let resumed = runs
        .begin_run(&alpha, "r2", EventSeq::new(11))
        .await
        .unwrap();
    assert_eq!(resumed.status, RunStatus::Running);
    assert_eq!(
        resumed.started_at_millis, parked.started_at_millis,
        "a resume keeps the moment the attempt first started"
    );
    runs.finish_run(&alpha, "r2", RunOutcome::new(RunStatus::WaitingApproval))
        .await
        .unwrap();
    runs.begin_run(&alpha, "r2", EventSeq::new(12))
        .await
        .unwrap();

    // Waiting-on-a-person can become waiting-on-something-else without a
    // terminal hop in between.
    runs.finish_run(&alpha, "r2", RunOutcome::new(RunStatus::Paused))
        .await
        .unwrap();
    let repark = runs
        .finish_run(&alpha, "r2", RunOutcome::new(RunStatus::WaitingApproval))
        .await
        .unwrap();
    assert_eq!(repark.status, RunStatus::WaitingApproval);

    // …and finally settles for good, carrying its reason.
    let failed = runs
        .finish_run(
            &alpha,
            "r2",
            RunOutcome::new(RunStatus::Failed).with_error("the tool never came back"),
        )
        .await
        .unwrap();
    assert_eq!(failed.status, RunStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("the tool never came back"));
    assert!(failed.finished_at_millis.is_some());

    // A run that never started can still settle — the shape a boot reaper and a
    // dispatch that died before its first turn both need.
    let never_ran = runs
        .finish_run(
            &alpha,
            "r3",
            RunOutcome::new(RunStatus::Cancelled).with_error("the operator withdrew the card"),
        )
        .await
        .unwrap();
    assert_eq!(never_ran.status, RunStatus::Cancelled);
    assert_eq!(never_ran.started_at_millis, None);

    // -- the step trace ------------------------------------------------------

    let step = |run_id: &str, seq: u32, label: &str| RunStepRecord {
        run_id: run_id.to_string(),
        step_seq: seq,
        at_millis: 1_000 + u64::from(seq),
        step: TurnStep {
            kind: TurnStepKind::ToolCall,
            status: TurnStepStatus::Ok,
            label: label.to_string(),
            detail: None,
            elapsed_ms: Some(5),
            ..TurnStep::default()
        },
    };

    assert!(
        runs.list_run_steps(&alpha, "r1").await.unwrap().is_empty(),
        "a run with no trace reads back empty, not missing"
    );

    runs.append_run_step(&alpha, &step("r1", 0, "Reading messages"))
        .await
        .unwrap();
    runs.append_run_step(&alpha, &step("r1", 1, "Thinking"))
        .await
        .unwrap();
    runs.append_run_step(&alpha, &step("r1", 2, "Writing the reply"))
        .await
        .unwrap();
    // A different run's trace must not bleed into this one.
    runs.append_run_step(&alpha, &step("r4", 0, "Somebody else's step"))
        .await
        .unwrap();
    // …nor another company's.
    runs.append_run_step(&beta, &step("r1", 0, "Beta's step"))
        .await
        .unwrap();

    let trace = runs.list_run_steps(&alpha, "r1").await.unwrap();
    assert_eq!(trace.len(), 3);
    assert_eq!(
        trace.iter().map(|s| s.step_seq).collect::<Vec<_>>(),
        [0, 1, 2],
        "steps read back in run order, oldest first"
    );
    assert_eq!(trace[1].step.label, "Thinking");
    assert_eq!(trace[0], step("r1", 0, "Reading messages"));

    assert_eq!(runs.list_run_steps(&alpha, "r4").await.unwrap().len(), 1);
    let beta_trace = runs.list_run_steps(&beta, "r1").await.unwrap();
    assert_eq!(beta_trace.len(), 1);
    assert_eq!(beta_trace[0].step.label, "Beta's step");

    // Re-appending the same `(run_id, step_seq)` overwrites: a retried write
    // must not duplicate a step.
    runs.append_run_step(&alpha, &step("r1", 1, "Thinking harder"))
        .await
        .unwrap();
    let trace = runs.list_run_steps(&alpha, "r1").await.unwrap();
    assert_eq!(
        trace.len(),
        3,
        "an idempotent append does not grow the trace"
    );
    assert_eq!(trace[1].step.label, "Thinking harder");

    // -- filters and ordering ------------------------------------------------

    let all = runs.list_runs(&alpha, &RunFilter::default()).await.unwrap();
    assert_eq!(all.len(), 4, "r1..r4");

    let for_card = runs
        .list_runs(&alpha, &RunFilter::for_task("card"))
        .await
        .unwrap();
    assert_eq!(
        for_card.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["r3", "r2", "r1"],
        "one card's attempts come back newest first"
    );

    let succeeded = runs
        .list_runs(
            &alpha,
            &RunFilter::default().with_status(RunStatus::Succeeded),
        )
        .await
        .unwrap();
    assert_eq!(succeeded.len(), 1);
    assert_eq!(succeeded[0].id, "r1");

    let settled_two = runs
        .list_runs(
            &alpha,
            &RunFilter::default()
                .with_status(RunStatus::Failed)
                .with_status(RunStatus::Cancelled),
        )
        .await
        .unwrap();
    let mut ids: Vec<&str> = settled_two.iter().map(|r| r.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["r2", "r3"], "a multi-status filter is a union");

    // Task and status compose.
    let none = runs
        .list_runs(
            &alpha,
            &RunFilter::for_task("other").with_status(RunStatus::Succeeded),
        )
        .await
        .unwrap();
    assert!(none.is_empty());

    // The limit truncates the newest end, after ordering.
    let capped = runs
        .list_runs(&alpha, &RunFilter::for_task("card").with_limit(2))
        .await
        .unwrap();
    assert_eq!(
        capped.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["r3", "r2"]
    );
    assert!(
        runs.list_runs(&alpha, &RunFilter::default().with_limit(0))
            .await
            .unwrap()
            .is_empty()
    );

    // A filter that matches nothing is empty, not an error.
    assert!(
        runs.list_runs(&alpha, &RunFilter::for_task("no-such-card"))
            .await
            .unwrap()
            .is_empty()
    );

    // -- list_stale_active ---------------------------------------------------

    // r1 Succeeded, r2 Failed, r3 Cancelled, r4 still Pending.
    let stale = runs.list_stale_active(&alpha).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, "r4");
    assert_eq!(stale[0].status, RunStatus::Pending);

    runs.begin_run(&alpha, "r4", EventSeq::new(20))
        .await
        .unwrap();
    let stale = runs.list_stale_active(&alpha).await.unwrap();
    assert_eq!(stale.len(), 1, "Running is active too");

    runs.finish_run(&alpha, "r4", RunOutcome::new(RunStatus::WaitingApproval))
        .await
        .unwrap();
    assert!(
        runs.list_stale_active(&alpha).await.unwrap().is_empty(),
        "parked is not active: a run waiting on a person is not stale"
    );

    // -- a run at no card (issue #983) ---------------------------------------
    //
    // The chat-turn shape: an attempt at work that opened no board card. It has
    // to round-trip, it has to be reachable, and — the part a backend gets wrong
    // by accident — it must not answer a per-card filter, because `task_id`
    // being absent is not the same as it matching.

    let chat = runs
        .create_run(&alpha, NewRun::for_chat("t1", "general", "ceo"))
        .await
        .unwrap();
    assert_eq!(chat.task_id, None);
    assert_eq!(chat.chat_id.as_deref(), Some("general"));
    assert_eq!(
        chat.attempt, 1,
        "with no card there is nothing for a second attempt to be the second of"
    );
    assert_eq!(runs.get_run(&alpha, "t1").await.unwrap(), Some(chat));

    let second_chat = runs
        .create_run(&alpha, NewRun::for_chat("t2", "general", "ceo"))
        .await
        .unwrap();
    assert_eq!(
        second_chat.attempt, 1,
        "card-less runs do not share one anonymous attempt counter"
    );

    for card in ["card", "other", "no-such-card"] {
        let matched = runs
            .list_runs(&alpha, &RunFilter::for_task(card))
            .await
            .unwrap();
        assert!(
            !matched.iter().any(|r| r.id == "t1" || r.id == "t2"),
            "a card-less run answered the filter for card '{card}'"
        );
    }
    let all = runs.list_runs(&alpha, &RunFilter::default()).await.unwrap();
    assert!(
        all.iter().any(|r| r.id == "t1"),
        "an unfiltered list must still reach a card-less run"
    );

    // It moves through the state machine like any other row, so the orphan
    // reaper and the terminality backstop need no card-less special case.
    runs.begin_run(&alpha, "t1", EventSeq::new(31))
        .await
        .unwrap();
    assert_eq!(
        runs.list_stale_active(&alpha)
            .await
            .unwrap()
            .iter()
            .filter(|r| r.id == "t1")
            .count(),
        1,
        "a running card-less run is active"
    );
    let settled = runs
        .finish_run(&alpha, "t1", RunOutcome::new(RunStatus::Succeeded))
        .await
        .unwrap();
    assert_eq!(settled.status, RunStatus::Succeeded);
    assert_eq!(settled.task_id, None, "the settle invented no card");
}

/// Asserts a [`RunRecord`](crate::ports::runs::RunRecord) written before
/// `task_id` could be absent still loads (issue #983).
///
/// This is a pure serde property, so it is asserted once here rather than per
/// backend: all three store the record as the same JSON blob, so a row written
/// by a pre-#983 host is a `"taskId": "<string>"` whichever backend holds it.
/// It is in the conformance module because that is where the round-trip
/// contract lives, and because a backend that ever stops using the record's own
/// serialization is exactly what this would catch.
pub fn assert_legacy_run_row_loads() {
    use crate::ports::runs::{RunRecord, RunStatus};

    let legacy = serde_json::json!({
        "id": "run-1",
        "company": "acme",
        "taskId": "card-7",
        "agentId": "ceo",
        "attempt": 2,
        "status": "running",
        "createdAtMillis": 1_700_000_000_000u64,
    });
    let loaded: RunRecord = serde_json::from_value(legacy).expect("a pre-#983 row still loads");
    assert_eq!(loaded.task_id.as_deref(), Some("card-7"));
    assert_eq!(loaded.chat_id, None, "an absent conversation reads as None");
    assert_eq!(loaded.status, RunStatus::Running);

    // And the new shape omits the key rather than writing `null`, so a dispatch
    // row is byte-identical to the ones already on disk.
    let card_less = RunRecord {
        task_id: None,
        chat_id: Some("general".to_string()),
        ..loaded
    };
    let written = serde_json::to_value(&card_less).unwrap();
    assert!(
        written.get("taskId").is_none(),
        "an absent card must be omitted, never null: {written}"
    );
    assert_eq!(
        serde_json::from_value::<RunRecord>(written).unwrap(),
        card_less,
        "the card-less row round-trips"
    );
}

/// Asserts the boot-reaper contract
/// ([`reap_orphaned_runs`](crate::ports::runs::reap_orphaned_runs)): every run
/// left `Pending` or `Running` by a dead process is failed with the orphan
/// reason, and every parked run is left exactly as it was.
pub async fn assert_run_reaper(runs: Arc<dyn crate::ports::runs::RunStore>) {
    use crate::ports::runs::{NewRun, ORPHAN_ERROR, RunFilter, RunOutcome, RunStatus};

    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let spec = |id: &str, task: &str| NewRun::for_task(id, task, "ceo");

    // One of each state the reaper has an opinion about.
    runs.create_run(&alpha, spec("pending", "a")).await.unwrap();

    runs.create_run(&alpha, spec("running", "b")).await.unwrap();
    runs.begin_run(&alpha, "running", EventSeq::new(1))
        .await
        .unwrap();

    runs.create_run(&alpha, spec("review", "c")).await.unwrap();
    runs.begin_run(&alpha, "review", EventSeq::new(2))
        .await
        .unwrap();
    runs.finish_run(
        &alpha,
        "review",
        RunOutcome::new(RunStatus::WaitingApproval),
    )
    .await
    .unwrap();

    runs.create_run(&alpha, spec("paused", "d")).await.unwrap();
    runs.begin_run(&alpha, "paused", EventSeq::new(3))
        .await
        .unwrap();
    runs.finish_run(&alpha, "paused", RunOutcome::new(RunStatus::Paused))
        .await
        .unwrap();

    runs.create_run(&alpha, spec("done", "e")).await.unwrap();
    runs.begin_run(&alpha, "done", EventSeq::new(4))
        .await
        .unwrap();
    runs.finish_run(&alpha, "done", RunOutcome::new(RunStatus::Succeeded))
        .await
        .unwrap();

    // Another company's stranded run must survive alpha's sweep.
    runs.create_run(&beta, spec("beta-pending", "a"))
        .await
        .unwrap();

    let reaped = crate::ports::runs::reap_orphaned_runs(runs.as_ref(), &alpha)
        .await
        .unwrap();
    assert_eq!(reaped.len(), 2, "exactly the Pending and Running rows");
    // Issue #337: the caller gets the records, not a count, because it has to
    // return each reaped run's *card* to To-do — and for that it needs the
    // `task_id`s. A count would leave the board claiming work nothing is doing.
    let mut reaped_tasks: Vec<&str> = reaped.iter().filter_map(|r| r.task_id.as_deref()).collect();
    reaped_tasks.sort_unstable();
    assert_eq!(
        reaped_tasks,
        ["a", "b"],
        "the cards of the pending and running rows"
    );

    let status = |id: &'static str| {
        let runs = runs.clone();
        let alpha = alpha.clone();
        async move { runs.get_run(&alpha, id).await.unwrap().unwrap() }
    };

    let pending = status("pending").await;
    assert_eq!(pending.status, RunStatus::Failed);
    assert_eq!(pending.error.as_deref(), Some(ORPHAN_ERROR));
    assert!(pending.finished_at_millis.is_some());

    assert_eq!(status("running").await.status, RunStatus::Failed);

    // Parked is not orphaned — reaping these would delete real pending work on
    // every restart.
    assert_eq!(status("review").await.status, RunStatus::WaitingApproval);
    assert_eq!(status("review").await.error, None);
    assert_eq!(status("paused").await.status, RunStatus::Paused);

    // A terminal run is untouched, and keeps its own outcome.
    assert_eq!(status("done").await.status, RunStatus::Succeeded);
    assert_eq!(status("done").await.error, None);

    // Isolation: beta's stranded run is still stranded.
    assert_eq!(
        runs.get_run(&beta, "beta-pending")
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Pending
    );

    // The sweep is idempotent: a second boot finds nothing left to reclaim.
    assert!(
        crate::ports::runs::reap_orphaned_runs(runs.as_ref(), &alpha)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        runs.list_runs(&alpha, &RunFilter::active())
            .await
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// ScheduleFireStore (issue #241)
// ---------------------------------------------------------------------------
//
// Imports for this suite are function-local rather than added to the module
// header, keeping it a pure append to a file edited concurrently on other
// branches.

/// Asserts the
/// [`ScheduleFireStore`](crate::ports::schedule_fires::ScheduleFireStore)
/// contract: a claim is won exactly once; keys are isolated per minute, per
/// schedule and per company; `latest_fire` is the max claimed minute (never the
/// last written); pruning removes only rows strictly below the cutoff and never
/// the anchor; and N concurrent claimers of one key produce exactly one winner —
/// the cross-replica race the whole port exists to arbitrate.
pub async fn assert_schedule_fire_store(
    fires: Arc<dyn crate::ports::schedule_fires::ScheduleFireStore>,
) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    // -- first claim wins, a repeat loses -----------------------------------

    assert!(
        fires.claim_fire(&alpha, "workflow-a", 100).await.unwrap(),
        "the first claim on a key wins"
    );
    assert!(
        !fires.claim_fire(&alpha, "workflow-a", 100).await.unwrap(),
        "a second claim on the same key loses"
    );

    // -- keys are distinct per minute, per schedule, per company ------------

    assert!(
        fires.claim_fire(&alpha, "workflow-a", 101).await.unwrap(),
        "a different minute is a different claim"
    );
    assert!(
        fires.claim_fire(&alpha, "workflow-b", 100).await.unwrap(),
        "a different schedule at the same minute is a different claim"
    );
    assert!(
        fires.claim_fire(&beta, "workflow-a", 100).await.unwrap(),
        "another company claiming the same key does not collide"
    );

    // -- latest_fire is the max claimed minute, or None ---------------------

    assert_eq!(
        fires.latest_fire(&alpha, "workflow-a").await.unwrap(),
        Some(101),
        "the anchor is the highest claimed minute"
    );
    assert_eq!(
        fires.latest_fire(&alpha, "workflow-b").await.unwrap(),
        Some(100)
    );
    assert_eq!(
        fires.latest_fire(&beta, "workflow-a").await.unwrap(),
        Some(100),
        "company A's rows are invisible to company B's anchor"
    );
    assert_eq!(
        fires.latest_fire(&alpha, "never").await.unwrap(),
        None,
        "a schedule that never fired has no anchor"
    );

    // Claiming an OLDER minute after a newer one does not move the anchor down:
    // it is a max, not a last-write.
    assert!(fires.claim_fire(&alpha, "workflow-a", 50).await.unwrap());
    assert_eq!(
        fires.latest_fire(&alpha, "workflow-a").await.unwrap(),
        Some(101)
    );

    // -- prune removes only rows strictly below the cutoff, never the anchor -
    //
    // Prune is COMPANY-wide across every schedule, not per schedule. alpha holds
    // workflow-a {50, 100, 101} and workflow-b {100}. Pruning below 101 drops
    // workflow-a's 50 and 100 and workflow-b's 100 — three rows — and keeps
    // workflow-a's 101, exactly the anchor-preservation invariant the 14-day
    // cutoff / 7-day window gap guarantees in production.
    let removed = fires.prune_fires_before(&alpha, 101).await.unwrap();
    assert_eq!(
        removed, 3,
        "prune removes every row below the cutoff, across all schedules"
    );
    assert_eq!(
        fires.latest_fire(&alpha, "workflow-a").await.unwrap(),
        Some(101),
        "the newest row survives a prune whose cutoff equals it"
    );
    assert_eq!(
        fires.latest_fire(&alpha, "workflow-b").await.unwrap(),
        None,
        "a schedule whose only row fell below the cutoff has no anchor left"
    );
    // A pruned minute no longer exists, so it can be claimed again.
    assert!(
        fires.claim_fire(&alpha, "workflow-a", 100).await.unwrap(),
        "a pruned minute can be re-claimed"
    );
    // Prune is per-company: beta's row is untouched.
    assert_eq!(
        fires.latest_fire(&beta, "workflow-a").await.unwrap(),
        Some(100)
    );
    // A cutoff below everything removes nothing.
    assert_eq!(fires.prune_fires_before(&alpha, 0).await.unwrap(), 0);

    // -- N concurrent claimers of one key: exactly one winner ---------------
    //
    // Spawned tasks, so the claims genuinely contend rather than serialising on
    // one await. This is the property hosted replicas depend on: two processes
    // ticking the same minute must not both fire.
    const N: usize = 16;
    let key_minute = 777_u64;
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..N {
        let fires = fires.clone();
        let company = alpha.clone();
        set.spawn(async move {
            fires
                .claim_fire(&company, "race", key_minute)
                .await
                .unwrap()
        });
    }
    let mut winners = 0;
    while let Some(res) = set.join_next().await {
        if res.unwrap() {
            winners += 1;
        }
    }
    assert_eq!(
        winners, 1,
        "exactly one of {N} concurrent claimers may win the key"
    );

    // -- delete_schedule_fires: purge one schedule's whole ledger (issue #708) --
    //
    // Fresh schedule ids so this block never disturbs the prune-count assertions
    // above. `del-a` gets three minutes, `del-b` one, and beta a same-named row,
    // to prove the delete is scoped to exactly `(company, schedule_id)`.
    for m in [200_u64, 201, 202] {
        assert!(fires.claim_fire(&alpha, "del-a", m).await.unwrap());
    }
    assert!(fires.claim_fire(&alpha, "del-b", 200).await.unwrap());
    assert!(fires.claim_fire(&beta, "del-a", 200).await.unwrap());

    let removed = fires.delete_schedule_fires(&alpha, "del-a").await.unwrap();
    assert_eq!(
        removed, 3,
        "delete removes every row for the schedule, whatever its minute"
    );
    assert_eq!(
        fires.latest_fire(&alpha, "del-a").await.unwrap(),
        None,
        "a purged schedule has no anchor left — the recreate starts fresh"
    );
    // The recreate case: a purged minute is no longer claimed, so the first tick
    // after a same-id recreate wins it again instead of losing to a stale claim.
    assert!(
        fires.claim_fire(&alpha, "del-a", 201).await.unwrap(),
        "a purged minute is claimable again (delete+recreate fires, not suppressed)"
    );

    // Scoped: the sibling schedule and the other company are untouched.
    assert_eq!(
        fires.latest_fire(&alpha, "del-b").await.unwrap(),
        Some(200),
        "a sibling schedule's rows survive another schedule's delete"
    );
    assert_eq!(
        fires.latest_fire(&beta, "del-a").await.unwrap(),
        Some(200),
        "company A's delete never reaches company B's identically-named schedule"
    );

    // A never-fired id removes nothing, and the call is idempotent.
    assert_eq!(
        fires
            .delete_schedule_fires(&alpha, "del-never")
            .await
            .unwrap(),
        0,
        "deleting a schedule that never fired removes nothing"
    );
    assert_eq!(
        fires.delete_schedule_fires(&alpha, "del-b").await.unwrap(),
        1,
        "del-b's single row is removed"
    );
    assert_eq!(
        fires.delete_schedule_fires(&alpha, "del-b").await.unwrap(),
        0,
        "a second delete of the same schedule is idempotent — nothing left to remove"
    );
}

/// Every backend's [`JournalStore`](crate::ports::journal::JournalStore) must
/// keep opaque lines byte-identically, in append order, per company (#726).
///
/// The runtime journal carries the at-most-once effect set and the durable
/// approval queue, and it decides what a line *means* above this port — so all a
/// backend owes is bytes and order. Both halves are load-bearing:
///
/// * **Bytes.** A line the store rewrote, trimmed or re-encoded is a record
///   `serde_json` no longer parses, which the journal reports as corruption and
///   skips. A skipped `EffectExecuted` un-commits its key and lets an
///   at-most-once effect fire a second time.
/// * **Order.** Replay folds records in sequence. A park read back *after* the
///   resolution that drains it resurrects a resolved approval.
///
/// Isolation is asserted for the same reason it is everywhere else, with a
/// sharper consequence here: one company reading another's executed keys would
/// suppress its own effects.
pub async fn assert_journal_store(journal: Arc<dyn crate::ports::journal::JournalStore>) {
    use crate::ports::journal::Durability;

    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    assert!(
        journal.read_journal(&alpha).await.unwrap().is_empty(),
        "a company that has never journaled reads back nothing"
    );

    // Deliberately awkward payloads: a backend that stores these unchanged is not
    // quietly normalising anything. No `\n` anywhere — the port's contract is one
    // record per call, and the caller never puts a terminator inside a line.
    let lines = [
        r#"{"record":"EffectExecuted","key":"cyc:0"}"#,
        r#"{"record":"EffectExecuted","key":"cyc:1","effect":{"kind":"payment.send"}}"#,
        r#"  {"record":"leading and trailing space"}  "#,
        "{\"record\":\"unicode\",\"memo\":\"caf\u{e9} \u{2014} \u{65e5}\u{672c}\u{8a9e}\t tabbed\"}",
        "not json at all, and it must survive anyway",
        "",
    ];
    // Both durability levels, alternating: a backend that honours only one of
    // them (or ignores the parameter) must still store and order every record
    // identically, and one that errored on the level it does not implement would
    // fail here rather than in production.
    for (n, line) in lines.iter().enumerate() {
        let durability = if n % 2 == 0 {
            Durability::Host
        } else {
            Durability::Process
        };
        journal
            .append_journal(&alpha, line, durability)
            .await
            .unwrap();
    }

    assert_eq!(
        journal.read_journal(&alpha).await.unwrap(),
        lines,
        "every line must read back byte-identically and in append order"
    );

    // Isolation, both ways.
    journal
        .append_journal(&beta, "beta-only", Durability::Host)
        .await
        .unwrap();
    assert_eq!(
        journal.read_journal(&beta).await.unwrap(),
        vec!["beta-only".to_string()],
        "one company's journal must hold only its own records"
    );
    assert_eq!(
        journal.read_journal(&alpha).await.unwrap().len(),
        lines.len(),
        "and another company's append must not land in it"
    );

    // Appending after a read keeps going from the end, not from zero — a backend
    // whose sequence restarted would overwrite the first record.
    journal
        .append_journal(&alpha, "later", Durability::Process)
        .await
        .unwrap();
    let after = journal.read_journal(&alpha).await.unwrap();
    assert_eq!(after.len(), lines.len() + 1);
    assert_eq!(after.last().unwrap(), "later", "and it lands at the end");
}

/// The one-time filesystem import and the receipt that gates it (#726).
///
/// Only for backends that can actually *hold* an import — sqlite and mongodb.
/// The fs backend reports itself permanently imported (its store is the file an
/// import would copy from), so running this against it would assert nothing.
///
/// The receipt is not bookkeeping. `complete_import` **clears** before it copies,
/// so a second import deletes every record the backend accumulated after the
/// first one — un-committing effect keys that have already run. And it records
/// the receipt **last**, so an import interrupted anywhere leaves the gate open
/// and the next boot re-runs the whole copy rather than resuming into a truncated
/// prefix. A truncated prefix is the failure this port exists to prevent.
pub async fn assert_journal_import(journal: Arc<dyn crate::ports::journal::JournalStore>) {
    use crate::ports::journal::Durability;

    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    assert!(
        !journal.journal_imported(&alpha).await.unwrap(),
        "a company the backend has never seen has not been imported"
    );

    let source = vec![
        r#"{"record":"EffectExecuted","key":"cyc:0"}"#.to_string(),
        r#"{"record":"EffectExecuted","key":"cyc:1"}"#.to_string(),
        "a corrupt line, migrated byte-for-byte".to_string(),
    ];
    journal
        .complete_import(&alpha, source.clone())
        .await
        .unwrap();

    assert_eq!(
        journal.read_journal(&alpha).await.unwrap(),
        source,
        "the import copies verbatim and in file order"
    );
    assert!(
        journal.journal_imported(&alpha).await.unwrap(),
        "and closes the gate"
    );
    assert!(
        !journal.journal_imported(&beta).await.unwrap(),
        "the receipt is per company, not per database"
    );

    // Appends after the import continue the sequence rather than colliding with
    // (or overwriting) the copied records.
    journal
        .append_journal(&alpha, "after-import", Durability::Host)
        .await
        .unwrap();
    let after = journal.read_journal(&alpha).await.unwrap();
    assert_eq!(
        after.len(),
        source.len() + 1,
        "an append after the import must not collide with a copied record's key"
    );
    assert_eq!(after[..source.len()], source[..]);
    assert_eq!(after.last().unwrap(), "after-import");

    // A retry — the shape of an import interrupted before its receipt — replaces
    // rather than appends.
    journal
        .complete_import(&alpha, source.clone())
        .await
        .unwrap();
    assert_eq!(
        journal.read_journal(&alpha).await.unwrap(),
        source,
        "a re-run import clears the partial copy; it must never append a second one"
    );

    // The empty import is how a company with no prior filesystem journal closes
    // its gate, and it must be a real (clearing) import, not a skipped no-op.
    journal.complete_import(&beta, Vec::new()).await.unwrap();
    assert!(journal.journal_imported(&beta).await.unwrap());
    assert!(journal.read_journal(&beta).await.unwrap().is_empty());
}
