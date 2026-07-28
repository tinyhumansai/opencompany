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

use crate::ports::context::ContextStore;
use crate::ports::events::EventLog;
use crate::ports::facts::{FactKind, FactRecord, FactStore};
use crate::ports::inbox::{EmailRecord, InboxMeta, InboxStore};
use crate::ports::login_codes::{LoginCodeRecord, LoginCodeStore};
use crate::ports::memory::MemoryStore;
use crate::ports::now_millis;
use crate::ports::sessions::{SessionRecord, SessionStore};
use crate::ports::skills_state::{SkillSource, SkillState, SkillStateStore};
use crate::ports::store::CompanyStore;
use crate::ports::tasks::{TaskRecord, TaskStore};
use crate::ports::types::{
    CompanyEvent, CompanyId, CompanyRecord, CompressedTrace, ContextChunk, EventSeq, LedgerEntry,
};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::ports::users::{InviteRecord, UserRecord, UserRole, UserStatus, UserStore};
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceStore};

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

/// Builds an empty running record for `id`.
fn record(id: &CompanyId) -> CompanyRecord {
    CompanyRecord {
        id: id.clone(),
        manifest: sample_manifest(),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
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
                text: "a".into(),
                by: None,
                chat: None,
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
                text: "e0".into(),
                by: None,
                chat: None,
            },
        )
        .await
        .unwrap();
    let s1 = events
        .append(
            &id,
            CompanyEvent::OperatorMessage {
                text: "e1".into(),
                by: None,
                chat: None,
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
                text: "e2".into(),
                by: None,
                chat: None,
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
                    text: format!("a{expected}"),
                    by: None,
                    chat: None,
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
                text: "b0".into(),
                by: None,
                chat: None,
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
            text: format!("event {i}"),
            by: None,
            chat: None,
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
    };

    tasks
        .upsert(&alpha, &task("t1", "backlog", 1))
        .await
        .unwrap();
    tasks
        .upsert(&alpha, &task("t2", "backlog", 2))
        .await
        .unwrap();
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

    assert!(users.delete_invite(&alpha, "i1").await.unwrap());
    assert!(!users.delete_invite(&alpha, "i1").await.unwrap());
    assert!(users.list_invites(&alpha).await.unwrap().is_empty());
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
/// rename+move (with cycle rejection), recursive delete, and the seeding gate.
pub async fn assert_workspace_store(ws: Arc<dyn WorkspaceStore>) {
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");
    let node = |id: &str, name: &str, kind: NodeKind, parent: Option<&str>| WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind,
        parent_id: parent.map(str::to_string),
        updated_at_millis: now_millis(),
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

    // Overwrite content.
    ws.write(&alpha, "note", "# Voice v2").await.unwrap();
    assert_eq!(
        ws.read(&alpha, "note").await.unwrap().unwrap().1,
        "# Voice v2"
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
