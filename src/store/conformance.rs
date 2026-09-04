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
//!
//! **Fixtures are non-empty on purpose.** An empty vec, map or `None` survives
//! every possible bug, including a backend that never persisted the field at
//! all, so seeding one certifies the gap it was meant to close. Issue #1504 was
//! exactly that: `overlay_agents` seeded as `Vec::new()` with no assertion, so a
//! backend that dropped every console-created teammate passed the whole suite.
//!
//! **No credential material appears here.** [`assert_secret_store`] uses
//! obviously fake placeholder values (`sk-not-a-real-key-…`).

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
use crate::ports::tasks::{TaskOrigin, TaskRecord, TaskStore, TaskTitle};
use crate::ports::types::{
    Attachment, ChunkAddr, ChunkMeta, CompanyEvent, CompanyId, CompanyRecord, CompressedTrace,
    ContextChunk, EventSeq, LedgerEntry, SecretValue, TemplateProvenance,
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
        auto_approve_under_usd: Some(Some(25.0)),
        approval_ttl_hours: Some(48),
        set_by: Actor {
            kind: ActorKind::User,
            id: "user-conformance".to_string(),
        },
        at_millis: 1_700_000_000_002,
    }
}

/// The console-created teammates the fixture seeds every record with, so each
/// backend (fs, sqlite, mongodb) proves an operator-added agent survives
/// persistence (issue #1504).
///
/// This overlay is the **only** copy of such a teammate — it is deliberately not
/// written back into the version-controlled `company.toml` — so a backend that
/// dropped it would delete the teammate on the next restart, and on a hosted
/// tenant there would be nothing to restore from.
///
/// Deliberately **three** rows that differ in their optional fields, because the
/// field's whole point is that those states stay apart across a round-trip. Since
/// issue #1804 `tools` is a three-state grant, and all three must survive:
///
/// - `aria_stone` has a `description` and a **narrowed** `tools` grant
///   (`Some(globs)`). Both are `skip_serializing_if`-elided when absent, so a
///   backend that persisted only the required `id`/`name`/`role` triple would
///   still round-trip a bare agent and pass. This row is what makes that fail.
/// - `pax_ivory` has `description: None` and an **absent** `tools` grant
///   (`None`), which means the standard company-wide grant — the teammate keeps
///   tracking `[tools].allow` (see
///   [`OverlayAgent::tools`](crate::ports::types::OverlayAgent::tools)). A
///   backend that rehydrated the absent key as `Some(vec![])` would silently
///   demote that teammate to a deny-all belt.
/// - `nix_slate` has an **explicit empty** `tools` grant (`Some(vec![])`), which
///   since #1804 is a deliberate **deny-all** — the opposite of `None`. A backend
///   that collapsed `Some(vec![])` into `None` (or dropped the empty array) would
///   silently re-grant that teammate the whole company belt.
fn sample_overlay_agents() -> Vec<crate::ports::types::OverlayAgent> {
    use crate::ports::types::OverlayAgent;
    vec![
        OverlayAgent {
            id: "aria_stone".to_string(),
            name: "Aria Stone".to_string(),
            role: "Head of Support".to_string(),
            description: Some("Answers customer mail and escalates refunds.".to_string()),
            tools: Some(vec!["docs.*".to_string(), "web".to_string()]),
            // Both set, so a backend that drops either fails here — the same
            // reason `tools` is a narrowed `Some` here, `None` on the next, and
            // an explicit empty `Some(vec![])` on the third.
            model: Some("claude-sonnet-4".to_string()),
            harness: Some("claude".to_string()),
        },
        OverlayAgent {
            id: "pax_ivory".to_string(),
            name: "Pax Ivory".to_string(),
            role: "Analyst".to_string(),
            description: None,
            // `None` = inherit the standard company-wide grant. Must rehydrate
            // as `None`, never as `Some(vec![])` (which since #1804 is deny-all).
            tools: None,
            // The absent half of the pair: `None` must rehydrate as `None`,
            // never as an empty string pinning the teammate to a nameless
            // harness.
            model: None,
            harness: None,
        },
        OverlayAgent {
            id: "nix_slate".to_string(),
            name: "Nix Slate".to_string(),
            role: "Contractor".to_string(),
            description: None,
            // Explicit empty list = deliberate deny-all (issue #1804), the
            // opposite of `None`. Must survive as `Some(vec![])`, never collapse
            // to `None` (which would silently re-grant the whole company belt).
            tools: Some(Vec::new()),
            model: None,
            harness: None,
        },
    ]
}

/// The console-created desk the fixture seeds every record with, so each backend
/// proves an operator-created group chat survives persistence (issue #1504).
///
/// Its `members` name one manifest agent (`ceo`) and one overlay agent
/// (`aria_stone`), which is the mixed case a real console desk produces, and
/// `description` is `Some` so the `skip_serializing_if` field is exercised.
///
/// Two desks since issue #1835, one per responder mode: `support` never states
/// a mode (the defaulted-and-skipped half — a pre-#1835 row must rehydrate as
/// `Lead`), and `launch` is an `auto` channel, so every backend proves the
/// mode a rail-created channel stores actually survives persistence rather
/// than silently collapsing back to a lead desk.
fn sample_overlay_desks() -> Vec<crate::ports::types::OverlayDesk> {
    use crate::ports::types::{OverlayDesk, ResponderMode};
    vec![
        OverlayDesk {
            id: "support".to_string(),
            name: "Support".to_string(),
            description: Some("Customer mail triage.".to_string()),
            members: vec!["ceo".to_string(), "aria_stone".to_string()],
            responder: ResponderMode::default(),
        },
        OverlayDesk {
            id: "launch".to_string(),
            name: "Launch week".to_string(),
            description: None,
            members: vec!["ceo".to_string(), "aria_stone".to_string()],
            responder: ResponderMode::Auto,
        },
    ]
}

/// The console-added desk memberships the fixture seeds every record with, so
/// each backend proves an operator's "add to desk" survives persistence
/// (issue #1504). Targets the manifest-shaped desk id used by the desk-order
/// overlay, so the two overlays are proven to persist independently.
fn sample_overlay_desk_members() -> Vec<crate::ports::types::OverlayDeskMember> {
    use crate::ports::types::OverlayDeskMember;
    vec![OverlayDeskMember {
        desk_id: "studio".to_string(),
        agent_id: "pax_ivory".to_string(),
    }]
}

/// The operator's edits of a manifest-declared teammate: a renamed role, a
/// cleared description (the empty-string form), a narrowed tool scope and a
/// chosen face, so a backend that drops the field — or that collapses "cleared"
/// back into "not overridden" — is caught by the round-trip rather than in a
/// console that silently re-inherits the blueprint after a restart.
fn sample_agent_overrides() -> Vec<crate::ports::types::AgentOverride> {
    vec![crate::ports::types::AgentOverride {
        agent_id: "ceo".to_string(),
        name: Some("Robin".to_string()),
        role: Some("Chief Vibes".to_string()),
        description: Some(String::new()),
        tools: Some(Some(vec!["docs.*".to_string()])),
        instructions: Some("Be exceedingly concise and decisive.".to_string()),
        // A dropped avatar reads as "nobody has chosen", so the teammate's face
        // would silently revert to the hashed default on the next restart — the
        // same class of loss as re-inheriting the blueprint role.
        avatar: Some("tiny:violet".to_string()),
        // Set rather than defaulted: this fixture exists to prove a store
        // round-trips the whole override, so every field it gains needs a real
        // value here or the new ones are covered by nothing.
        model: Some("claude-opus-4-5".to_string()),
        harness: Some("laptop".to_string()),
    }]
}

/// Builds a running record for `id` carrying a non-empty desk-order overlay (so
/// the store round-trip covers the operator desk-hierarchy field, issue #131), a
/// runtime-authored workflow body (issue #168), a populated budget-override set
/// (issue #343), a `[policy]` override (issue #562), a paused workflow id
/// (issue #276), console-created teammates, desks and desk memberships
/// (issue #1504), and stamped with the sample template provenance (so round-trips
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
        // Non-empty for the same reason `overlay_desk_tools` below is: an empty
        // vec survives every possible bug, including not persisting the field at
        // all. Issue #1504 — this was the one overlay field left empty, so a
        // backend that dropped console-created teammates passed the whole suite.
        overlay_agents: sample_overlay_agents(),
        overlay_desk_members: sample_overlay_desk_members(),
        overlay_desk_order: vec![crate::ports::types::OverlayDeskOrder {
            desk_id: "studio".to_string(),
            ordered: vec!["ceo".to_string(), "eng".to_string()],
        }],
        overlay_desks: sample_overlay_desks(),
        overlay_workflows: vec![sample_overlay_workflow()],
        overlay_budgets: sample_budget_overrides(),
        overlay_policy: Some(sample_policy_override()),
        // Non-empty for the same reason: this is the one overlay that WIDENS
        // `[tools].allow`, so a backend that drops it silently revokes an
        // integration the operator granted from a connect surface and leaves
        // the restored company "Connected" and reaching nobody (issue #1796).
        overlay_tool_grants: Some(crate::ports::types::ToolGrantsOverride {
            added: vec!["chargebee".to_string()],
            set_by: crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::User,
                id: "admin@example.com".to_string(),
            },
            at_millis: 1_700_000_000_000,
        }),
        // Non-empty so a backend that drops the field is caught: an empty map
        // survives every possible bug, including not persisting it at all.
        overlay_desk_tools: std::collections::BTreeMap::from([(
            "studio".to_string(),
            vec!["docs.*".to_string(), "web".to_string()],
        )]),
        disabled_workflows: vec!["digest".to_string()],
        template_provenance: Some(sample_provenance()),
        setup: Some(sample_setup_answers()),
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
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
                mentions: Vec::new(),
                parent: None,
                text: "a".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
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
    // `beta` was never saved: the activation gate reads "never seen", exactly
    // like a company with no bundle/document/row at all.
    assert!(
        !store.activation_gate_seen(&beta).await.unwrap(),
        "a company that was never saved must report the activation gate as \
         never seen"
    );
    // PR #1875 review finding: `alpha` WAS just saved, by this same
    // activation-aware build, so the gate must already read "seen" —
    // immediately, with no second save. A backend that leaves this at the
    // `CompanyStore` trait's always-`false` default cannot tell a fresh
    // company's second boot apart from a genuine pre-#1843 legacy record, and
    // `RuntimeBuilder::build`'s grandfather back-fill would silently
    // auto-activate every such company on that backend the moment it
    // restarts before onboarding finishes — the exact bug #1843 fixed,
    // reopened for whichever backend forgets this.
    assert!(
        store.activation_gate_seen(&alpha).await.unwrap(),
        "a company just saved by activation-aware code must have the \
         activation gate marked as seen — a backend inheriting the trait's \
         always-false default would re-open the #1843 auto-activation bug"
    );

    // `alpha` still sees its own data.
    let loaded = store.load(&alpha).await.unwrap().expect("alpha record");
    assert_eq!(loaded.ledger.len(), 1);
    // The console-created teammates survive the store round-trip (issue #1504).
    // Until this assertion existed the fixture seeded an empty vec, so a backend
    // that never persisted the field passed the entire suite — and the overlay
    // is the only copy of an operator-added teammate, so losing it deletes them.
    assert_eq!(
        loaded.overlay_agents,
        sample_overlay_agents(),
        "overlay_agents did not survive save/load"
    );
    // Spelled out per optional field, because equality alone would not say which
    // half broke and both halves are elided from the persisted JSON when empty.
    let aria = loaded
        .overlay_agents
        .iter()
        .find(|agent| agent.id == "aria_stone")
        .expect("the console-created teammate survived save/load");
    assert_eq!(
        aria.description,
        Some("Answers customer mail and escalates refunds.".to_string()),
        "the teammate's mandate decayed into an absent description"
    );
    assert_eq!(
        aria.tools,
        Some(vec!["docs.*".to_string(), "web".to_string()]),
        "the teammate's narrowed tool grant decayed into the standard company grant"
    );
    let pax = loaded
        .overlay_agents
        .iter()
        .find(|agent| agent.id == "pax_ivory")
        .expect("the standard-grant teammate survived save/load");
    assert_eq!(
        pax.tools, None,
        "the standard-grant teammate (None) came back as a narrowed or deny-all belt"
    );
    let nix = loaded
        .overlay_agents
        .iter()
        .find(|agent| agent.id == "nix_slate")
        .expect("the deny-all teammate survived save/load");
    assert_eq!(
        nix.tools,
        Some(Vec::new()),
        "the deny-all teammate (Some(vec![])) collapsed into None and silently \
         re-gained the whole company grant"
    );
    // And the round-tripped teammate is on the roster, which is what the overlay
    // is for: a backend could persist the rows and still fail to make them count.
    assert!(
        loaded.is_roster_agent("aria_stone"),
        "the console-created teammate did not rejoin the roster after save/load"
    );
    // The operator-created desk survives too (issue #1504) — the desk analogue
    // of the teammate overlay, and equally the only copy of that group chat.
    assert_eq!(
        loaded.overlay_desks,
        sample_overlay_desks(),
        "overlay_desks did not survive save/load"
    );
    assert!(
        loaded.desk_exists("support"),
        "the console-created desk did not survive save/load"
    );
    // As does the operator's "add this teammate to that desk" (issue #1504),
    // which is a separate overlay and can be dropped on its own.
    assert_eq!(
        loaded.overlay_desk_members,
        sample_overlay_desk_members(),
        "overlay_desk_members did not survive save/load"
    );
    assert!(
        loaded
            .effective_desk_members("studio")
            .contains(&"pax_ivory".to_string()),
        "the console-added desk membership did not survive save/load"
    );
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

/// PR #1875 review finding: `CompanyStore::save` stamps `activation_gate_seen:
/// true` unconditionally, on the reasoning (its own doc comment) that "every
/// OTHER call site really is activation-aware code doing a normal write" —
/// true for a `running` company, but not for one still `paused` on its first
/// post-upgrade boot. `RuntimeBuilder::build`'s own "existing but not
/// running" arm already knows this and deliberately leaves the marker exactly
/// as recorded rather than migrating a paused legacy record — but that
/// protection only covers saves `build` itself makes. Any OTHER ordinary
/// write against the same still-paused, not-yet-migrated record — e.g.
/// `company_logo::put_logo`'s plain load-modify-save cycle, which does not
/// check lifecycle at all — used to stamp the marker `true` regardless,
/// poisoning it before the company's own first `running` boot ever gets to
/// decide. Once poisoned, the grandfather arm's `!gate_already_seen` guard
/// can never fire again, and a genuinely legacy operator who resumes their
/// paused company is shown the fresh-company onboarding funnel instead of
/// being grandfathered in.
pub async fn assert_paused_ordinary_save_preserves_activation_gate(store: Arc<dyn CompanyStore>) {
    let id = CompanyId::new("paused-legacy");
    let mut paused = record(&id);
    paused.lifecycle = "paused".to_string();

    // Simulate a legacy pre-#1843 bundle that is still unmigrated:
    // `activation_gate_seen` explicitly `false`, exactly like a record no
    // activation-aware `build` has ever decided.
    store.save_importing(&paused, false).await.unwrap();
    assert!(
        !store.activation_gate_seen(&id).await.unwrap(),
        "setup: the fixture must start gate-unseen"
    );

    // An ordinary write against the still-paused record — a console route
    // like `company_logo::put_logo` that loads, mutates one field, and calls
    // plain `save`, with no lifecycle check of its own.
    paused.manifest.company.logo_url = Some("data:image/png;base64,AA==".to_string());
    store.save(&paused).await.unwrap();

    assert!(
        !store.activation_gate_seen(&id).await.unwrap(),
        "an ordinary write against a still-paused, not-yet-migrated legacy \
         record must not stamp the activation gate marker `true` — only a \
         `running` boot's own migration decision may, or a resumed legacy \
         company is shown onboarding it should have been grandfathered past"
    );

    // Once the company is actually running, an ordinary save is still free to
    // stamp the marker — the common case `save`'s `true` exists for, which
    // the fix above must not break.
    let mut running = paused.clone();
    running.lifecycle = "running".to_string();
    store.save(&running).await.unwrap();
    assert!(
        store.activation_gate_seen(&id).await.unwrap(),
        "an ordinary write against a running company must still mark the \
         activation gate as seen"
    );
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
                mentions: Vec::new(),
                parent: None,
                text: "e0".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();
    let s1 = events
        .append(
            &id,
            CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "e1".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
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
                mentions: Vec::new(),
                parent: None,
                text: "e2".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
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
                    mentions: Vec::new(),
                    parent: None,
                    text: format!("a{expected}"),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
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
                mentions: Vec::new(),
                parent: None,
                text: "b0".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
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
                    mentions: Vec::new(),
                    parent: None,
                    text: format!("event {seq}"),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
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
                        mentions: Vec::new(),
                        parent: None,
                        text: text.to_string(),
                        by: None,
                        chat: None,
                        deliverable: None,
                        attachments: Vec::new(),
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

    // Issue #1890 G. `usize::MAX` is the port's "no limit" sentinel, and the
    // one input a backend is most likely to get wrong while looking correct:
    // an implementation that reserves against the limit allocates 2^64 slots,
    // and one that reads from the end must not treat it as a stopping count.
    // Every caller of the unbounded form is a full-history reader, so a page
    // silently short here is a reader silently missing history.
    let all = events.read_before(&id, None, usize::MAX).await.unwrap();
    assert_eq!(
        all.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![seqs[3], seqs[2], seqs[1], seqs[0]],
        "an unlimited page is the whole log, newest-first"
    );
    let unbounded_before = events
        .read_before(&id, Some(seqs[2]), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        unbounded_before
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![seqs[1], seqs[0]],
        "…and still stops at the cursor"
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
        started_by: None,
        resume_semantic: None,
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
        // Issue #1682: one fixture carries a populated attachment so the
        // totality round-trip can catch a backend that drops the field —
        // every empty-`Vec::new()` fixture above would still pass one, since
        // an empty list is indistinguishable from a missing one after
        // deserialization. Event 2 keeps the exact metadata (including the
        // server-extracted text) so the byte-identical replay below pins it.
        let attachments = if i == 2 {
            vec![Attachment {
                node_id: "node-attach-0".to_string(),
                name: "Q3-report.pdf".to_string(),
                mime: "application/pdf".to_string(),
                size: 48_932,
                extracted_text: Some("Q3 revenue grew 12% year over year.".to_string()),
            }]
        } else {
            Vec::new()
        };
        let ev = CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: format!("event {i}"),
            by: None,
            chat: None,
            deliverable: None,
            attachments,
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
    // Issue #1504: the console-created teammates, desks and desk memberships
    // round-trip on every backend. An export that dropped them would lose the
    // only copy of every teammate the operator added outside the manifest.
    assert_eq!(
        loaded.overlay_agents,
        sample_overlay_agents(),
        "overlay_agents did not round-trip through the store"
    );
    assert_eq!(
        loaded.overlay_desks,
        sample_overlay_desks(),
        "overlay_desks did not round-trip through the store"
    );
    assert_eq!(
        loaded.overlay_desk_members,
        sample_overlay_desk_members(),
        "overlay_desk_members did not round-trip through the store"
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
    // Issue #1682: the populated-attachment event's metadata survives the
    // round-trip explicitly, not just as an equality side-effect — a backend
    // that drops `attachments` (or loses the extracted text) fails here.
    match &appended[2] {
        CompanyEvent::OperatorMessage { attachments, .. } => {
            assert_eq!(attachments.len(), 1);
            assert_eq!(attachments[0].node_id, "node-attach-0");
            assert_eq!(attachments[0].name, "Q3-report.pdf");
            assert_eq!(attachments[0].mime, "application/pdf");
            assert_eq!(attachments[0].size, 48_932);
            assert_eq!(
                attachments[0].extracted_text.as_deref(),
                Some("Q3 revenue grew 12% year over year.")
            );
        }
        _ => unreachable!("fixture event 2 is an OperatorMessage"),
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
        title: TaskTitle::authored(&format!("title {id}")),
        note: Some(format!("note {id}")),
        column: col.to_string(),
        priority: "medium".to_string(),
        assignee: "Strategy desk".to_string(),
        updated_at_millis: at,
        origin: None,
        parent_task_id: None,
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
        origin_message_seq: None,
        bounced: None,
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

    // Issue #1865: seed every recently-added optional field with a meaningful
    // value. An empty/`None` fixture would let a backend silently drop the
    // bounced marker, output lineage, or workflow proposal without failing.
    let populated = TaskRecord {
        note: Some("retry after the transport failed".to_string()),
        origin: TaskOrigin::new(
            Some("chat-1".to_string()),
            Some(crate::ports::EventSeq::new(41)),
        ),
        parent_task_id: Some("parent-1".to_string()),
        output: Some(crate::ports::tasks::TaskOutput {
            source: crate::ports::tasks::TaskOutputSource::Run {
                run_id: "run-1".to_string(),
                attempt: Some(2),
            },
            at_millis: 10,
            artifacts: vec![crate::ports::tasks::TaskOutputArtifact {
                artifact_id: "artifact-1".to_string(),
                version: 3,
                title: "Release notes".to_string(),
                kind: crate::ports::ArtifactKind::Markdown,
            }],
            workflows: vec![crate::ports::tasks::TaskOutputWorkflow {
                workflow_id: "release".to_string(),
                run_id: Some("run-1".to_string()),
                action: crate::ports::tasks::TaskOutputAction::Ran,
            }],
        }),
        deliverable: crate::ports::tasks::TaskDeliverable::Workflow,
        workflow_proposal: Some(crate::ports::tasks::TaskWorkflowProposal {
            summary: "Publish the release notes".to_string(),
            ops: serde_json::json!({"id": "release", "nodes": []}),
            generated_at_millis: 11,
            run_id: "run-1".to_string(),
        }),
        origin_run_id: Some("run-1".to_string()),
        origin_workflow_id: Some("release".to_string()),
        bounced: Some("the previous dispatch failed".to_string()),
        ..task("t-populated", "todo", 12)
    };
    tasks.upsert(&alpha, &populated).await.unwrap();
    let populated_back = tasks
        .list(&alpha)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == populated.id)
        .expect("the populated card persists");
    assert_eq!(
        populated_back, populated,
        "all populated fields must survive"
    );

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
        // Non-`None` so a backend that drops the column is caught here: a lost
        // avatar reads as "never chose one", so the person's face would revert
        // to the hashed default on the next read with nothing reporting it.
        avatar: Some("tiny:indigo".to_string()),
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
    // The whole record round-trips, not only the columns each backend happened
    // to think of: a dropped display name or avatar silently reverts a person
    // to the console's derived name and hashed face.
    assert_eq!(
        users.get_user(&alpha, "u1").await.unwrap().as_ref(),
        Some(&user("u1", "ada@example.com", 1))
    );
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

/// Asserts the [`SecretStore`] contract: read-back, absence, overwrite, per-key
/// independence, and — the property with security consequences — isolation
/// between companies (issue #1505).
///
/// Before this function existed the port had **no** conformance case on any
/// backend, while holding a tenant's inference credential, its MCP OAuth tokens,
/// its Composio connected-account tokens and its SMTP password. A backend that
/// failed to persist, failed to scope a read by company, or returned a stale
/// value after an overwrite would have passed the entire suite. On a hosted
/// deployment the storage backend *is* the tenant boundary, so "all three
/// behave identically here" is a security guarantee, not a tidiness one.
///
/// The port intentionally exposes no `delete` — callers clear a secret by
/// writing an empty value (`src/company/mcp.rs::clear_auth`,
/// `src/company/inference.rs::clear_key`) — so there is no deletion case, and
/// the empty-value case below is the one that stands in for it.
///
/// Keys are the real shapes the runtime uses (`inference/key`,
/// `mcp/<name>/auth`, `harness/<id>/…`). **Every value is an obviously fake
/// placeholder**; nothing here resembles a live credential.
///
pub async fn assert_secret_store(secrets: Arc<dyn crate::ports::secrets::SecretStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    // Compare the exposed `&str` rather than `Option<SecretValue>` so an
    // assertion message never carries a `SecretValue`'s `Debug` rendering.
    let read = |company: &CompanyId, key: &'static str| {
        let secrets = secrets.clone();
        let company = company.clone();
        async move {
            secrets
                .get(&company, key)
                .await
                .unwrap()
                .map(|value| value.expose().to_string())
        }
    };

    // 1. An unset key reads `None` — not an empty string, and not an error.
    assert_eq!(
        read(&alpha, "inference/key").await,
        None,
        "an unset secret must read as absent"
    );

    // 2. Write, then read back byte-identically. The value carries a newline and
    //    a non-ASCII character on purpose: the filesystem backend writes the raw
    //    bytes to a file, so a backend that appended a trailing newline, trimmed
    //    one, or round-tripped through a lossy encoding fails here.
    let token = "sk-not-a-real-key-alpha\nline2 café";
    secrets
        .set(&alpha, "inference/key", SecretValue(token.to_string()))
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "inference/key").await.as_deref(),
        Some(token),
        "a stored secret did not read back byte-identically"
    );

    // 3. Distinct keys are independent. `mcp/<name>/auth` and `mcp/<name>/health`
    //    are the real pair the MCP surface writes, and only one of them is a
    //    credential — a backend that conflated them would serve a scrubbed
    //    health record where a token belongs, or worse, the reverse.
    secrets
        .set(
            &alpha,
            "mcp/conformance/auth",
            SecretValue("{\"bearer\":\"not-a-real-token\"}".to_string()),
        )
        .await
        .unwrap();
    secrets
        .set(
            &alpha,
            "mcp/conformance/health",
            SecretValue("{\"ok\":true}".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "mcp/conformance/auth").await.as_deref(),
        Some("{\"bearer\":\"not-a-real-token\"}"),
        "writing a second key overwrote the first"
    );
    assert_eq!(
        read(&alpha, "mcp/conformance/health").await.as_deref(),
        Some("{\"ok\":true}"),
        "the second key did not persist alongside the first"
    );
    assert_eq!(
        read(&alpha, "inference/key").await.as_deref(),
        Some(token),
        "writing unrelated keys disturbed an existing secret"
    );

    // 4. Keys remain distinct even when a filesystem-safe filename needs to
    // encode them differently. MCP server names are trimmed but otherwise
    // accepted verbatim, so these are two valid server credential keys.
    secrets
        .set(
            &alpha,
            "mcp/acme prod/auth",
            SecretValue("token-for-space-name".to_string()),
        )
        .await
        .unwrap();
    secrets
        .set(
            &alpha,
            "mcp/acme_prod/auth",
            SecretValue("token-for-underscore-name".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "mcp/acme prod/auth").await.as_deref(),
        Some("token-for-space-name"),
        "a distinct key with a space was overwritten by its underscore variant"
    );
    assert_eq!(
        read(&alpha, "mcp/acme_prod/auth").await.as_deref(),
        Some("token-for-underscore-name"),
        "a distinct key with an underscore was overwritten by its space variant"
    );

    // Letter case is the same hazard one level down: on case-insensitive
    // volumes (macOS, Windows) `mcp/Acme/auth` and `mcp/acme/auth` are one
    // path, and a backend that let them share a file would overwrite one
    // server's credential with another's. `validate_servers` treats them as
    // two valid names.
    secrets
        .set(
            &alpha,
            "mcp/Acme/auth",
            SecretValue("token-for-upper-case".to_string()),
        )
        .await
        .unwrap();
    secrets
        .set(
            &alpha,
            "mcp/acme/auth",
            SecretValue("token-for-lower-case".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "mcp/Acme/auth").await.as_deref(),
        Some("token-for-upper-case"),
        "a distinct key that differs only in case was overwritten by its lower-case variant"
    );
    assert_eq!(
        read(&alpha, "mcp/acme/auth").await.as_deref(),
        Some("token-for-lower-case"),
        "a distinct key that differs only in case was overwritten by its upper-case variant"
    );

    // Windows strips trailing periods from a path component, so `foo` and
    // `foo.` are one directory entry there — the filesystem backend has to
    // encode the trailing dot, and every backend has to keep the two keys
    // apart regardless.
    secrets
        .set(&alpha, "foo", SecretValue("token-for-plain".to_string()))
        .await
        .unwrap();
    secrets
        .set(
            &alpha,
            "foo.",
            SecretValue("token-for-trailing-dot".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "foo").await.as_deref(),
        Some("token-for-plain"),
        "a distinct key ending in a period was overwritten by its plain variant"
    );
    assert_eq!(
        read(&alpha, "foo.").await.as_deref(),
        Some("token-for-trailing-dot"),
        "a distinct key ending in a period lost its own value"
    );

    // A key that is itself shaped like a legacy filename must not alias another
    // key. On the filesystem backend the canonical namespace and the legacy
    // slug fallback have to stay disjoint, so `key-foo` must not read the value
    // written for `foo`, and writing `key-foo` must not touch `foo`.
    secrets
        .set(&alpha, "foo", SecretValue("value-for-foo".to_string()))
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "key-foo").await,
        None,
        "reading a key-shaped legacy slug reached another key's value"
    );
    secrets
        .set(
            &alpha,
            "key-foo",
            SecretValue("value-for-key-foo".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "foo").await.as_deref(),
        Some("value-for-foo"),
        "writing a key-shaped legacy slug deleted another key"
    );

    // 5. Overwrite replaces. A backend that appended, or that served a cached or
    //    stale row, would hand a rotated credential's *predecessor* to the next
    //    outbound call — which fails as an authentication error days later, far
    //    from the rotation that caused it.
    secrets
        .set(
            &alpha,
            "inference/key",
            SecretValue("sk-not-a-real-key-alpha-rotated".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "inference/key").await.as_deref(),
        Some("sk-not-a-real-key-alpha-rotated"),
        "an overwritten secret still reads as its previous value"
    );

    // 6. Clearing writes an empty value, and an empty value is NOT absence. This
    //    distinction is load-bearing: `clear_auth`/`clear_key` clear a credential
    //    by writing `""`, and a backend that collapsed that into "unset" would
    //    fall back to whatever the manifest or the environment supplies — so the
    //    operator's revocation would silently not take.
    secrets
        .set(&alpha, "mcp/conformance/auth", SecretValue(String::new()))
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "mcp/conformance/auth").await.as_deref(),
        Some(""),
        "a cleared secret decayed into an unset one, so the revocation did not take"
    );

    // 7. ISOLATION — the property with security consequences. `beta` has written
    //    nothing, and must not observe `alpha`'s credentials under any key.
    for key in [
        "inference/key",
        "mcp/conformance/auth",
        "mcp/conformance/health",
    ] {
        assert_eq!(
            read(&beta, key).await,
            None,
            "company beta read company alpha's secret at `{key}` — a cross-tenant \
             credential disclosure"
        );
    }

    // 8. And the isolation holds in both directions once `beta` writes the SAME
    //    key: neither company sees the other's value. A backend that keyed only
    //    on `key` (dropping the company scope) passes step 6 and fails here,
    //    because until `beta` writes there is nothing for the missing scope to
    //    confuse.
    secrets
        .set(
            &beta,
            "inference/key",
            SecretValue("sk-not-a-real-key-beta".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        read(&beta, "inference/key").await.as_deref(),
        Some("sk-not-a-real-key-beta"),
        "beta could not read back its own secret"
    );
    assert_eq!(
        read(&alpha, "inference/key").await.as_deref(),
        Some("sk-not-a-real-key-alpha-rotated"),
        "beta's write overwrote alpha's secret at the same key — the company scope \
         is not part of the key"
    );

    // 9. A key that exists only for `beta` is still absent for `alpha`, which is
    //    the mirror of step 6 and catches a scope applied on write but not read.
    secrets
        .set(
            &beta,
            "harness/second/inference/key",
            SecretValue("sk-not-a-real-key-beta-second".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        read(&alpha, "harness/second/inference/key").await,
        None,
        "alpha read a secret only beta ever wrote"
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
/// stamp on the single-record backends (mongodb, the provider facade).
/// Readers of the stamp take the max across chunks for
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

/// Demands one search semantics under [`ContextStore::search`] from *every*
/// backend.
///
/// This assertion exists because `fs`, `sqlite` and `mongodb` each carried their
/// own copy of the search function, and all three copies were identically wrong:
/// a `body.find(query)` substring test scored 1.0, and truncation to `limit`
/// happened before any sorting. Three copies that agree by accident are three
/// copies that drift apart again — so the semantics is pinned here rather than
/// reviewed per backend.
///
/// What a store must do:
///
/// 1. **partial overlap counts**, because the memory loop searches with the
///    whole incoming message as its query and that never comes back verbatim;
/// 2. **the score ranks**, and rare words weigh more than words that appear
///    everywhere;
/// 3. **`limit` cuts *after* ranking**, not in read order;
/// 4. **no overlap is no hit**, so an empty result still means "there is nothing
///    here";
/// 5. the score stays inside `[0, 1]`, as [`ChunkHit`] promises.
pub async fn assert_context_search_ranking(context: Arc<dyn ContextStore>) {
    let alpha = CompanyId::new("alpha");
    let put = |label: &'static str, body: String| {
        let context = context.clone();
        let alpha = alpha.clone();
        async move {
            context
                .put(
                    &alpha,
                    ContextChunk {
                        label: label.to_string(),
                        body,
                    },
                )
                .await
                .unwrap()
        }
    };

    // Four older memories that share only the everyday words, and one that is
    // really about the subject. In read order the right one is last — exactly
    // the arrangement in which the old code returned the four oldest.
    let mut noise = Vec::new();
    for (label, body) in [
        (
            "task-outcome/a",
            "Task: put the minutes of the meeting in the folder\nOutcome: done",
        ),
        (
            "task-outcome/b",
            "Task: send the agenda for the week to the team\nOutcome: done",
        ),
        (
            "task-outcome/c",
            "Task: check the addresses in the list of customers\nOutcome: done",
        ),
        (
            "task-outcome/d",
            "Task: put Monday's review in the agenda\nOutcome: done",
        ),
    ] {
        noise.push(put(label, body.to_string()).await);
    }
    let target = put(
        "task-outcome/e",
        "Task: produce the quarterly overview of revenue for the north region\nOutcome: in the folder"
            .to_string(),
    )
    .await;

    // Today's question: same substance, different words. Under a substring test
    // this yields zero hits.
    let question =
        "produce the quarterly overview of revenue for the north region again for the customer";

    let all = context.search(&alpha, question, usize::MAX).await.unwrap();
    assert!(
        !all.is_empty(),
        "partial overlap must hit; a substring test returned nothing here"
    );
    assert_eq!(
        all[0].addr, target,
        "the memory sharing the rare words belongs on top"
    );
    for hit in &all {
        assert!(
            (0.0..=1.0).contains(&hit.score),
            "score outside the port contract [0,1]: {}",
            hit.score
        );
    }
    for hit in all.iter().skip(1) {
        assert!(
            hit.score < all[0].score,
            "a hit on everyday words alone must not tie with the real one"
        );
    }

    // `limit` must not cut in read order: with one slot the best must survive,
    // not the oldest.
    let one = context.search(&alpha, question, 1).await.unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(
        one[0].addr, target,
        "limit belongs after ranking, not before it"
    );

    // The snippet shows where the hit is, not the start of the chunk.
    assert!(
        one[0].snippet.contains("quarterly"),
        "the snippet must wrap the hit; got: {}",
        one[0].snippet
    );

    // No overlap stays no hit.
    assert!(
        context
            .search(&alpha, "shipbuilding", usize::MAX)
            .await
            .unwrap()
            .is_empty(),
        "without overlap nothing should come back"
    );

    // And the noise was not thrown away: it is still there, it just scores lower.
    assert_eq!(
        context.list(&alpha, "").await.unwrap().len(),
        noise.len() + 1
    );
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
        model: None,
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
        model: None,
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

    // Issue #1749: the model slug round-trips on every backend, and does so as
    // the *stored string* rather than only as a value that happens to compare
    // equal. `by model` is an aggregation over what came back out of the
    // store, so a backend that dropped or mangled the field would make the
    // whole question unanswerable while every other assertion here still
    // passed.
    let with_model = UsageSample {
        at_millis: 400,
        agent: "ceo".to_string(),
        provider: "byok".to_string(),
        input_tokens: 12,
        output_tokens: 4,
        cached_input_tokens: 0,
        cost_usd: 0.02,
        kind: SampleKind::Inference,
        run_id: None,
        model: Some(crate::metering::ModelSlug::classify(
            "anthropic/claude-sonnet-4-6",
        )),
    };
    usage.record(&alpha, &with_model).await.unwrap();
    let back = usage
        .query(&alpha, 400)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.at_millis == 400)
        .expect("the sample with a model persists");
    assert_eq!(back, with_model);
    assert_eq!(
        back.model.map(|m| m.as_str()),
        Some("anthropic-sonnet"),
        "the stored slug must survive the backend's own encoding"
    );
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
        model: None,
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
        // Company-wide, which is what every row written before the field
        // existed means — so the whole suite above this line is also the
        // regression guard for that reading.
        audience: None,
        context: None,
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

    // ---- Targeted rows: an audience is a boundary, not a hint ----
    //
    // A mention notification names the people it is for. Every backend must
    // enforce that identically, or one storage choice silently shows a person
    // a message they were never addressed by.
    let targeted = |id: &str, created_at: u64, audience: Option<Vec<&str>>| Notification {
        id: id.to_string(),
        kind: "mention".to_string(),
        subject: Subject {
            kind: SubjectKind::Message,
            id: "42".to_string(),
        },
        created_at,
        title: format!("someone mentioned you ({id})"),
        audience: audience.map(|a| a.into_iter().map(str::to_string).collect()),
        context: Some("engineering".to_string()),
    };

    let gamma = CompanyId::new("gamma");
    notes
        .append(&gamma, &targeted("for-ada", 100, Some(vec!["ada"])))
        .await
        .unwrap();
    notes
        .append(&gamma, &targeted("for-grace", 200, Some(vec!["grace"])))
        .await
        .unwrap();
    notes
        .append(&gamma, &targeted("for-everyone", 300, None))
        .await
        .unwrap();

    // Each person sees their own row plus the company-wide one, and nobody
    // else's.
    let ada = notes.list(&gamma, "ada").await.unwrap();
    let ada_ids: Vec<&str> = ada.iter().map(|v| v.notification.id.as_str()).collect();
    assert_eq!(ada_ids, vec!["for-everyone", "for-ada"]);
    let grace = notes.list(&gamma, "grace").await.unwrap();
    let grace_ids: Vec<&str> = grace.iter().map(|v| v.notification.id.as_str()).collect();
    assert_eq!(grace_ids, vec!["for-everyone", "for-grace"]);

    // A person named by nothing still sees the company-wide row — an audience
    // narrows, it does not opt anyone out of what was addressed to everybody.
    let stranger = notes.list(&gamma, "stranger").await.unwrap();
    assert_eq!(stranger.len(), 1);
    assert_eq!(stranger[0].notification.id, "for-everyone");

    // The context rides through, so a badge can be placed without the console
    // having loaded that channel's transcript.
    assert_eq!(ada[0].notification.context.as_deref(), Some("engineering"));
    assert_eq!(
        ada[1].notification.audience.as_deref(),
        Some(&["ada".to_string()][..])
    );

    // The unread count is per person AND per audience: Ada has two visible
    // rows, not three, so a badge built from this cannot count a colleague's
    // mention.
    let ada_unread = notes
        .mark_read(&gamma, "ada", Some(&["for-ada".to_string()]))
        .await
        .unwrap();
    assert_eq!(
        ada_unread, 1,
        "the company-wide row is still unread for Ada"
    );

    // Marking everything read is scoped the same way: it must not reach into
    // Grace's targeted row, and Grace must be unaffected.
    let ada_unread = notes.mark_read(&gamma, "ada", None).await.unwrap();
    assert_eq!(ada_unread, 0);
    let grace = notes.list(&gamma, "grace").await.unwrap();
    assert!(
        grace.iter().all(|v| v.read_at.is_none()),
        "Ada marking all read must not touch Grace's own rows"
    );

    // And a person cannot mark somebody else's row read by naming its id.
    let stranger_unread = notes
        .mark_read(&gamma, "stranger", Some(&["for-grace".to_string()]))
        .await
        .unwrap();
    assert_eq!(
        stranger_unread, 1,
        "the stranger's own unread count is the company-wide row alone"
    );
    let grace = notes.list(&gamma, "grace").await.unwrap();
    assert!(
        grace
            .iter()
            .find(|v| v.notification.id == "for-grace")
            .expect("grace still sees her row")
            .read_at
            .is_none(),
        "naming another person's notification must not mark it read for them"
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
        adopted: false,
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
/// [`WorkspaceStore::read_capped`] answers the length of every text body, and
/// hands back only the ones that fit.
///
/// The property that matters is what it does *not* return: a body over the cap
/// comes back empty, with its true length beside it, so a caller that would
/// discard it never receives it. Every backend has to be checked, because each
/// one measures differently — a `stat`, a SQL `length()`, an aggregation stage —
/// and only the contract is shared.
pub async fn assert_workspace_read_capped(ws: Arc<dyn WorkspaceStore>) {
    let company = CompanyId::new("capped-co");
    let operator = WorkspaceOrigin::Operator;
    let node = |id: &str, name: &str, kind: NodeKind, mime: Option<&str>| WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind,
        parent_id: None,
        updated_at_millis: now_millis(),
        created_by: operator.clone(),
        updated_by: operator.clone(),
        mime: mime.map(str::to_string),
        size: None,
        sha256: None,
        adopted: false,
    };

    // Multi-byte on purpose: the cap is bytes, and a backend that measures
    // characters would call this note shorter than it is.
    let small = "héllo wörld";
    let small_len = small.len() as u64;
    assert!(small_len > small.chars().count() as u64);
    ws.create(
        &company,
        &node("cap-small", "small.md", NodeKind::File, None),
        Some(small),
    )
    .await
    .expect("create the small note");

    let big = "x".repeat(4096);
    ws.create(
        &company,
        &node("cap-big", "big.md", NodeKind::File, None),
        Some(&big),
    )
    .await
    .expect("create the big note");

    ws.create(
        &company,
        &node("cap-empty", "empty.md", NodeKind::File, None),
        Some(""),
    )
    .await
    .expect("create the empty note");

    ws.create(
        &company,
        &node("cap-folder", "folder", NodeKind::Folder, None),
        None,
    )
    .await
    .expect("create the folder");

    ws.create_binary(
        &company,
        &node(
            "cap-blob",
            "blob.bin",
            NodeKind::File,
            Some("application/octet-stream"),
        ),
        &[0xff, 0xfe, 0x00, 0x01],
    )
    .await
    .expect("create the payload");

    // Under the cap: the body comes back whole, measured in bytes.
    let (_, body, len) = ws
        .read_capped(&company, "cap-small", 1024)
        .await
        .expect("read the small note")
        .expect("the small note exists");
    assert_eq!(body, small, "a body under the cap is returned in full");
    assert_eq!(len, small_len, "the length is bytes, not characters");

    // Over the cap: the length is still exact, and the body is withheld.
    let (_, body, len) = ws
        .read_capped(&company, "cap-big", 1024)
        .await
        .expect("read the big note")
        .expect("the big note exists");
    assert_eq!(
        len, 4096,
        "the true length is reported even when the body is not"
    );
    assert!(
        body.is_empty(),
        "a body over the cap must not be transferred"
    );

    // Exactly at the cap is under it, not over it.
    let (_, body, _) = ws
        .read_capped(&company, "cap-big", 4096)
        .await
        .expect("read at the cap")
        .expect("the big note exists");
    assert_eq!(body.len(), 4096, "a body exactly at the cap still fits");

    // An empty note and an over-cap note both answer an empty body; the length
    // is what tells them apart.
    let (_, body, len) = ws
        .read_capped(&company, "cap-empty", 1024)
        .await
        .expect("read the empty note")
        .expect("the empty note exists");
    assert!(body.is_empty());
    assert_eq!(len, 0);

    // A folder and a payload answer the same empty body `read` gives them.
    for id in ["cap-folder", "cap-blob"] {
        let (_, body, len) = ws
            .read_capped(&company, id, 1024)
            .await
            .expect("read")
            .unwrap_or_else(|| panic!("{id} exists"));
        assert!(body.is_empty(), "{id} must read as an empty body");
        assert_eq!(len, 0, "{id} must report no text length");
    }

    assert!(
        ws.read_capped(&company, "cap-missing", 1024)
            .await
            .expect("read a missing id")
            .is_none(),
        "an id naming nothing answers None, as `read` does"
    );

    // Company isolation, the same as every other read on this port.
    assert!(
        ws.read_capped(&CompanyId::new("capped-other"), "cap-small", 1024)
            .await
            .expect("read across companies")
            .is_none(),
        "another company's node must not be readable"
    );
}

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
        adopted: false,
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
            adopted: false,
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
        adopted: false,
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
        adopted: false,
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

/// The adoption lease every backend must honour (issue #1839).
///
/// #1801 gave the tree an empty-folder rollback: a folder one caller minted, then
/// failed to write beneath, is removed by
/// [`delete_if_empty`](WorkspaceStore::delete_if_empty). But a folder one caller
/// mints, a second caller can *adopt* — and the adopter has a legitimate reason
/// to write into it that the minter's rollback must not sweep away. The lease is
/// how the store records that second writer:
///
/// * an [`adopt_or_create_folder`](WorkspaceStore::adopt_or_create_folder) that
///   **adopts** stamps [`WorkspaceNode::adopted`], durably, before it returns;
/// * `delete_if_empty` refuses a folder carrying the flag even while it is still
///   childless — that is the whole point, since the adopter's write has not
///   landed yet.
///
/// A freshly minted folder does **not** carry it, so the rollback #1801 exists
/// for still works: a minted-unadopted-empty folder is deleted. This is what
/// keeps "swept a genuine leak" and "kept a folder someone else is writing into"
/// on opposite sides of one bit.
pub async fn assert_workspace_adoption_lease(ws: Arc<dyn WorkspaceStore>) {
    use crate::ports::workspace::FolderClaim;

    let alpha = CompanyId::new("alpha");
    let origin = WorkspaceOrigin::Agent {
        id: "cmo".to_string(),
    };

    // -- A minted, unadopted folder is not leased --------------------------
    let minted = ws
        .adopt_or_create_folder(&alpha, None, "task-A", origin.clone())
        .await
        .expect("a free name is claimable");
    assert!(minted.was_created(), "the name was free");
    assert!(
        !minted.node().adopted,
        "a freshly minted folder carries no adoption lease"
    );
    let minted_id = minted.node().id.clone();

    // -- A second claim adopts it, and stamps the lease durably ------------
    let adopted = ws
        .adopt_or_create_folder(&alpha, None, "task-A", origin.clone())
        .await
        .expect("an existing folder is adopted");
    assert!(!adopted.was_created(), "the folder was already there");
    assert!(
        matches!(adopted, FolderClaim::Adopted(_)),
        "a second claimer adopts rather than mints"
    );
    assert!(
        adopted.node().adopted,
        "adoption stamps the lease on the returned node"
    );
    assert_eq!(adopted.node().id, minted_id, "and it is the same folder");
    // The flag is persisted, not only present on the returned value — a fresh
    // read (the path `delete_if_empty` and the rollback take) must see it.
    let seen = ws
        .tree(&alpha)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.id == minted_id)
        .expect("the folder is in the tree");
    assert!(
        seen.adopted,
        "the lease survives a round trip through the store"
    );

    // -- delete_if_empty refuses the adopted-empty folder ------------------
    assert!(
        !ws.delete_if_empty(&alpha, &minted_id)
            .await
            .expect("delete_if_empty must not error on an adopted folder"),
        "an adopted folder is refused while still childless — its writer has not landed"
    );
    assert!(
        ws.tree(&alpha)
            .await
            .unwrap()
            .iter()
            .any(|n| n.id == minted_id),
        "and it must still be standing"
    );

    // -- but a minted, never-adopted empty folder still deletes ------------
    let leak = ws
        .adopt_or_create_folder(&alpha, None, "task-B", origin)
        .await
        .expect("a second free name is claimable");
    assert!(leak.was_created());
    let leak_id = leak.node().id.clone();
    assert!(
        ws.delete_if_empty(&alpha, &leak_id)
            .await
            .expect("delete_if_empty on an unadopted empty folder"),
        "a minted, unadopted, empty folder is the #1801 leak and must still be swept"
    );
    assert!(
        !ws.tree(&alpha)
            .await
            .unwrap()
            .iter()
            .any(|n| n.id == leak_id),
        "and it must be gone"
    );
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
        adopted: false,
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
        adopted: false,
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

/// A stat-then-open [`WorkspaceStore::read_capped`] measures a file's length
/// with one call and materializes its body with a second, so a concurrent
/// replacement can land between them: the length describes one revision and
/// the body handed back is another, larger, one — defeating the cap the
/// method exists to enforce. The fix has to answer from a single snapshot.
/// Every backend measures differently (a `stat`, a document field, ...), so
/// only the contract is shared: whatever `read_capped` returns, the body
/// never exceeds the cap, and when a body comes back, its length matches the
/// one reported beside it.
pub async fn assert_workspace_read_capped_race(ws: Arc<dyn WorkspaceStore>) {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// How many times the note is rewritten end to end.
    const ROUNDS: usize = 60;
    /// Concurrent readers. More than one, because a single reader spends much
    /// of its time not inside the window.
    const READERS: usize = 4;
    const MAX_BYTES: u64 = 300_000;

    let company = CompanyId::new("cap-race-co");
    // One revision fits under the cap, the other is well past it, so a length
    // measured against the wrong revision is caught either way: a stale
    // "small" length paired with the big body still overruns the cap, and a
    // stale "big" length paired with the small body still mismatches it.
    let small = "y".repeat(1_000);
    let big = "x".repeat(600_000);
    assert!((small.len() as u64) <= MAX_BYTES, "small must fit the cap");
    assert!((big.len() as u64) > MAX_BYTES, "big must exceed the cap");

    let node = WorkspaceNode {
        id: "race-note".to_string(),
        name: "Race.md".to_string(),
        kind: NodeKind::File,
        parent_id: None,
        updated_at_millis: now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    };
    ws.create(&company, &node, Some(&small))
        .await
        .expect("seed the note");

    let done = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let ws = Arc::clone(&ws);
            let company = company.clone();
            let done = Arc::clone(&done);
            tokio::spawn(async move {
                let mut observed = 0usize;
                while !done.load(Ordering::Relaxed) {
                    let (_, body, len) =
                        match ws.read_capped(&company, "race-note", MAX_BYTES).await {
                            Ok(Some(hit)) => hit,
                            Ok(None) => {
                                return Err("the note vanished; nothing in this test deletes it"
                                    .to_string());
                            }
                            Err(e) => {
                                return Err(format!(
                                    "a capped read concurrent with a write FAILED ({e})"
                                ));
                            }
                        };
                    if body.len() as u64 > MAX_BYTES {
                        return Err(format!(
                            "read_capped returned a {actual}-byte body against a {cap}-byte \
                             cap (reported length {len}) — a concurrent write defeated the cap.",
                            actual = body.len(),
                            cap = MAX_BYTES,
                        ));
                    }
                    if !body.is_empty() && body.len() as u64 != len {
                        return Err(format!(
                            "read_capped reported length {len} but returned a {actual}-byte \
                             body — length and body must describe the same snapshot.",
                            actual = body.len(),
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
        let body = if round % 2 == 0 { &big } else { &small };
        ws.write(&company, "race-note", body, WorkspaceOrigin::Operator)
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
        "no capped read ran while the note was being rewritten, so this case proved nothing"
    );
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
        adopted: false,
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

    // A by-design decline (issue #1809) is terminal and round-trips like any
    // other settle: neither an error nor a plain success, so the store must
    // persist and read it back exactly. Kept on its own company so it does not
    // perturb the r1..r4 list-count assertions below.
    let gamma = CompanyId::new("gamma");
    runs.create_run(&gamma, spec("g1", "card")).await.unwrap();
    let declined = runs
        .finish_run(
            &gamma,
            "g1",
            RunOutcome::new(RunStatus::Declined)
                .with_error("better done once than built into a workflow"),
        )
        .await
        .unwrap();
    assert_eq!(declined.status, RunStatus::Declined);
    assert!(
        declined.finished_at_millis.is_some(),
        "Declined is terminal, so it carries a finish time"
    );
    assert_eq!(
        runs.get_run(&gamma, "g1").await.unwrap(),
        Some(declined),
        "a declined run round-trips byte-identically"
    );

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

    // -- the per-desk filter (issue #1573) -----------------------------------
    //
    // In a company of its own so it disturbs none of the counts above, which
    // are asserted exactly.
    //
    // Three things have to hold, and only the first is obvious. A desk's
    // history spans **cards and card-less chat turns** alike, because both are
    // recorded attempts at work by that desk — a filter that saw only
    // dispatches would hide every conversation an operator had with the
    // teammate. It composes with the other predicates rather than replacing
    // them. And it stops at the company boundary like every other read here.
    let gamma = CompanyId::new("gamma");
    runs.create_run(&gamma, NewRun::for_task("g1", "card", "engineer"))
        .await
        .unwrap();
    runs.create_run(&gamma, NewRun::for_chat("g2", "general", "engineer"))
        .await
        .unwrap();
    runs.create_run(&gamma, NewRun::for_task("g3", "card", "ceo"))
        .await
        .unwrap();
    runs.create_run(&alpha, NewRun::for_task("g4", "card", "engineer"))
        .await
        .unwrap();

    let mut engineer = runs
        .list_runs(&gamma, &RunFilter::for_agent("engineer"))
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect::<Vec<_>>();
    // Sorted rather than compared in list order: all three rows are minted in
    // the same millisecond, so `sort_newest_first` breaks the tie on `attempt`
    // and `id`, and pinning that here would assert the tiebreak rather than the
    // filter. The ordering itself is already pinned above.
    engineer.sort();
    assert_eq!(
        engineer,
        ["g1", "g2"],
        "one desk's attempts, cards and card-less chat turns alike, and nobody \
         else's — not the other desk in this company, not the same desk in another"
    );

    assert_eq!(
        runs.list_runs(&gamma, &RunFilter::for_agent("ceo"))
            .await
            .unwrap()
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["g3"]
    );

    // Composes with `task_id`: the desk's attempts *at one card*.
    assert_eq!(
        runs.list_runs(
            &gamma,
            &RunFilter {
                agent_id: Some("engineer".into()),
                ..RunFilter::for_task("card")
            }
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.id.as_str())
        .collect::<Vec<_>>(),
        ["g1"],
        "the desk and card predicates intersect rather than either winning"
    );

    // …and with `statuses`. Every gamma row is still `Pending`.
    assert!(
        runs.list_runs(
            &gamma,
            &RunFilter::for_agent("engineer").with_status(RunStatus::Succeeded)
        )
        .await
        .unwrap()
        .is_empty()
    );

    // A desk nobody ran is empty, not an error — a removed teammate's id, or a
    // typo, must not look like a broken read.
    assert!(
        runs.list_runs(&gamma, &RunFilter::for_agent("nobody"))
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

/// The workflow-run join, on every backend.
///
/// Split out of [`assert_run_store`] rather than folded into it so a backend can
/// be brought up against the join alone, and so the assertion reads as one idea.
pub async fn assert_run_store_workflow_join(runs: Arc<dyn crate::ports::runs::RunStore>) {
    use crate::ports::runs::{NewRun, RunFilter};

    let alpha = CompanyId::new("alpha");

    // Two nodes of one workflow run, plus an ordinary card dispatch beside them.
    runs.create_run(
        &alpha,
        NewRun::for_workflow_node("w1", "wr-1", "solve", "programmer"),
    )
    .await
    .unwrap();
    runs.create_run(
        &alpha,
        NewRun::for_workflow_node("w2", "wr-1", "check", "verifier"),
    )
    .await
    .unwrap();
    runs.create_run(
        &alpha,
        NewRun::for_workflow_node("w3", "wr-2", "solve", "programmer"),
    )
    .await
    .unwrap();
    runs.create_run(&alpha, NewRun::for_task("t1", "card", "ceo"))
        .await
        .unwrap();

    // The fields round-trip.
    let one = runs.get_run(&alpha, "w1").await.unwrap().unwrap();
    assert_eq!(one.workflow_run_id.as_deref(), Some("wr-1"));
    assert_eq!(one.node_id.as_deref(), Some("solve"));
    assert_eq!(one.task_id, None, "a workflow node attempts no card");
    assert_eq!(one.chat_id, None, "and belongs to no conversation");

    // A card dispatch belongs to no workflow — `None` is true, not a placeholder.
    let card = runs.get_run(&alpha, "t1").await.unwrap().unwrap();
    assert_eq!(card.workflow_run_id, None);
    assert_eq!(card.node_id, None);

    // The filter selects exactly one run's nodes.
    let mine = runs
        .list_runs(&alpha, &RunFilter::for_workflow_run("wr-1"))
        .await
        .unwrap();
    let mut ids: Vec<&str> = mine.iter().map(|r| r.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["w1", "w2"], "only wr-1's nodes");

    // An unknown workflow run matches nothing rather than everything — the
    // failure mode of a filter that is silently dropped.
    assert!(
        runs.list_runs(&alpha, &RunFilter::for_workflow_run("nope"))
            .await
            .unwrap()
            .is_empty()
    );

    // An unfiltered list still sees them all, including the card dispatch.
    let all = runs.list_runs(&alpha, &RunFilter::default()).await.unwrap();
    assert_eq!(all.len(), 4);
}

/// Every backend's [`DeepTraceStore`](crate::ports::deep_trace::DeepTraceStore)
/// must agree on isolation, replacement, ordering, pruning and purge.
///
/// The contract this pins is narrow but load-bearing: the store holds secrets,
/// so "company A cannot see company B" and "purge really destroys" are not
/// niceties, and a backend that quietly diverges on either is a disclosure bug
/// rather than a bug.
pub async fn assert_deep_trace_store(deep: Arc<dyn crate::ports::deep_trace::DeepTraceStore>) {
    use crate::ports::deep_trace::{RunStepDetailRecord, TurnStepDetail};

    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    let record = |run: &str, seq: u32, at: u64, reasoning: &str| RunStepDetailRecord {
        run_id: run.to_string(),
        step_seq: seq,
        at_millis: at,
        detail: TurnStepDetail {
            reasoning: Some(reasoning.to_string()),
            ..TurnStepDetail::default()
        },
    };

    // -- a run with no detail reads as empty, not as an error ----------------

    assert!(
        deep.list_step_details(&alpha, "missing")
            .await
            .unwrap()
            .is_empty(),
        "a run that recorded nothing reads empty"
    );

    // -- append and read back, ordered by step_seq ---------------------------

    // Deliberately written out of order: the store orders, the caller does not.
    deep.append_step_detail(&alpha, &record("r1", 2, 20, "second"))
        .await
        .unwrap();
    deep.append_step_detail(&alpha, &record("r1", 0, 10, "first"))
        .await
        .unwrap();
    deep.append_step_detail(&alpha, &record("r1", 1, 15, "middle"))
        .await
        .unwrap();

    let got = deep.list_step_details(&alpha, "r1").await.unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(
        got.iter().map(|r| r.step_seq).collect::<Vec<_>>(),
        [0, 1, 2],
        "details come back in step order regardless of write order"
    );
    assert_eq!(got[0].detail.reasoning.as_deref(), Some("first"));
    assert_eq!(got[2].detail.reasoning.as_deref(), Some("second"));

    // -- replacement on (run_id, step_seq), not duplication ------------------

    // A reasoning run flushes partway and again at close under the same ordinal.
    deep.append_step_detail(&alpha, &record("r1", 1, 30, "middle, finished"))
        .await
        .unwrap();
    let got = deep.list_step_details(&alpha, "r1").await.unwrap();
    assert_eq!(got.len(), 3, "a re-write replaces rather than stacking");
    assert_eq!(
        got[1].detail.reasoning.as_deref(),
        Some("middle, finished"),
        "the later write is the truth"
    );

    // -- per-company isolation ----------------------------------------------

    deep.append_step_detail(&beta, &record("r1", 0, 10, "beta's secret"))
        .await
        .unwrap();
    let alpha_view = deep.list_step_details(&alpha, "r1").await.unwrap();
    assert_eq!(alpha_view.len(), 3, "beta's row is invisible to alpha");
    assert!(
        alpha_view
            .iter()
            .all(|r| r.detail.reasoning.as_deref() != Some("beta's secret")),
        "a run id shared across companies must not leak"
    );
    assert_eq!(deep.list_step_details(&beta, "r1").await.unwrap().len(), 1);

    // -- every field survives the round trip ---------------------------------

    let rich = RunStepDetailRecord {
        run_id: "r2".to_string(),
        step_seq: 0,
        at_millis: 99,
        detail: TurnStepDetail {
            reasoning: Some("why".to_string()),
            arguments: Some(r#"{"cmd":"ls"}"#.to_string()),
            output: Some("a\nb\n".to_string()),
            display_detail: Some("listing".to_string()),
            iteration: Some(3),
            clipped: true,
        },
    };
    deep.append_step_detail(&alpha, &rich).await.unwrap();
    assert_eq!(
        deep.list_step_details(&alpha, "r2").await.unwrap(),
        vec![rich],
        "read-back is byte-identical (the export-totality precondition)"
    );

    // -- purge one run leaves the others -------------------------------------

    let removed = deep.purge_deep_trace(&alpha, Some("r2")).await.unwrap();
    assert_eq!(removed, 1, "purge reports what it destroyed");
    assert!(
        deep.list_step_details(&alpha, "r2")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        deep.list_step_details(&alpha, "r1").await.unwrap().len(),
        3,
        "purging one run does not touch another"
    );
    assert_eq!(
        deep.list_step_details(&beta, "r1").await.unwrap().len(),
        1,
        "purging alpha does not touch beta"
    );

    // Purging what is already gone is not an error.
    assert_eq!(deep.purge_deep_trace(&alpha, Some("r2")).await.unwrap(), 0);

    // -- purge the whole company ---------------------------------------------

    let removed = deep.purge_deep_trace(&alpha, None).await.unwrap();
    assert_eq!(removed, 3);
    assert!(
        deep.list_step_details(&alpha, "r1")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        deep.list_step_details(&beta, "r1").await.unwrap().len(),
        1,
        "a company-wide purge is still scoped to that company"
    );
}
