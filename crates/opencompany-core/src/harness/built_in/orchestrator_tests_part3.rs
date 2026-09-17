use super::*;

/// A hand-off back to somebody already on the chain is refused as a cycle —
/// the A→B→A guard, at the boundary, in the model's own turn.
#[tokio::test]
async fn a_hand_off_back_up_the_teammate_chain_is_refused() {
    let company = CompanyId::new("acme");
    let queue = DelegationQueue::default();
    let _claim = queue.claim();
    let record = peers_record(&company);
    let tool = DelegateToTeammateTool::for_member(
        queue.clone(),
        company,
        Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>,
        MemberScope {
            member: "editor".to_string(),
            delegates_to: Vec::new(),
        },
    );
    // `editor` is running inside a hand-off `writer` made.
    let _scope = queue.enter_scope(crate::runtime::delegation_tools::teammate_scope_key(
        "writer",
    ));
    let result = tool
        .execute(json!({ "teammate": "writer", "instruction": "you take it back" }))
        .await
        .expect("execute");
    assert!(result.is_error, "{}", result.output_for_llm(true));
    assert!(
        result.output_for_llm(true).contains("loop"),
        "{}",
        result.output_for_llm(true)
    );
    assert_eq!(queue.queued(), 0);
}

/// The depth bound applies to the teammate hand-off exactly as it does to
/// the desk one — the guard a ring of three the cycle check cannot see still
/// runs into.
#[tokio::test]
async fn the_depth_bound_stops_a_teammate_hand_off_too() {
    let company = CompanyId::new("acme");
    let queue = DelegationQueue::default();
    let _claim = queue.claim();
    let mut record = peers_record(&company);
    record.manifest.tools.max_delegation_depth = Some(1);
    let tool = member_teammate_tool(record, &queue);
    let _scope = queue.enter_scope("strategy".to_string());
    let result = tool
        .execute(json!({ "teammate": "editor", "instruction": "tighten the copy" }))
        .await
        .expect("execute");
    assert!(result.is_error, "depth 1 must stop a further hand-off");
    assert!(
        result
            .output_for_llm(true)
            .contains("as far as this company allows"),
        "{}",
        result.output_for_llm(true)
    );
    assert_eq!(queue.queued(), 0);
}

/// The orchestrator's copy is unrestricted: it reaches a teammate that is
/// not a desk lead, with no allowlist in the way. Grounding still applies.
#[tokio::test]
async fn the_orchestrators_teammate_tool_is_unrestricted_but_grounded() {
    let company = CompanyId::new("acme");
    let queue = DelegationQueue::default();
    let _claim = queue.claim();
    let record = peers_record(&company);
    let store = Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>;
    let tool = DelegateToTeammateTool::new(queue.clone(), company, store);

    let ok = tool
        .execute(json!({ "teammate": "editor", "instruction": "tighten the copy" }))
        .await
        .expect("execute");
    assert!(!ok.is_error, "{}", ok.output_for_llm(true));

    let refused = tool
        .execute(json!({ "teammate": "ghost", "instruction": "do it" }))
        .await
        .expect("execute");
    assert!(refused.is_error, "{}", refused.output_for_llm(true));
}

/// Issue #1162, the other half of the fix: a hand-off written with a
/// teammate's **display name** is accepted, and what reaches the queue is
/// the **canonical id**.
///
/// Queueing the key as typed is what would make a name-accepting refusal
/// worse than the refusal it replaced — the tool would answer "Handed to
/// …" and the drain, which resolves independently, would find nothing to
/// deliver to. The reply names both strings so the model learns the id.
#[tokio::test]
async fn a_teammate_named_by_display_name_is_queued_under_its_id() {
    let company = CompanyId::new("acme");
    let queue = DelegationQueue::default();
    let _claim = queue.claim();
    let mut record = peers_record(&company);
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "dana_designer".to_string(),
        name: "Dana Designer".to_string(),
        role: "Designer".to_string(),
        description: None,
        tools: None,
        model: None,
        harness: None,
    });
    let store = Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>;
    let tool = DelegateToTeammateTool::new(queue.clone(), company, store);

    let result = tool
        .execute(json!({ "teammate": "Dana Designer", "instruction": "draw it" }))
        .await
        .expect("execute");
    assert!(!result.is_error, "{}", result.output_for_llm(true));
    let reply = result.output_for_llm(true);
    assert!(
        reply.contains("Dana Designer") && reply.contains("dana_designer"),
        "the reply must name the person and teach the id: {reply}"
    );
    assert_eq!(
        queue.drain(MAX_DELEGATIONS_PER_TURN),
        vec![Delegation::DelegateToTeammate {
            teammate: "dana_designer".to_string(),
            instruction: "draw it".to_string(),
        }],
        "the queue must carry the canonical id, not the key as typed"
    );
}

/// A display name two teammates answer to is refused rather than routed to
/// whichever was added first, and the refusal carries the ids to retry
/// with — the collision is the operator's to resolve, and the model cannot
/// do it without being told the alternatives (issue #1162).
#[tokio::test]
async fn a_display_name_two_teammates_share_is_refused_with_their_ids() {
    let company = CompanyId::new("acme");
    let queue = DelegationQueue::default();
    let _claim = queue.claim();
    let mut record = peers_record(&company);
    for id in ["dana_designer", "dana_designer_2"] {
        record.overlay_agents.push(OverlayAgent {
            provider: None,
            id: id.to_string(),
            name: "Dana Designer".to_string(),
            role: "Designer".to_string(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });
    }
    let store = Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>;
    let tool = DelegateToTeammateTool::new(queue.clone(), company, store);

    let result = tool
        .execute(json!({ "teammate": "Dana Designer", "instruction": "draw it" }))
        .await
        .expect("execute");
    assert!(result.is_error, "{}", result.output_for_llm(true));
    let refusal = result.output_for_llm(true);
    assert!(
        refusal.contains("dana_designer") && refusal.contains("dana_designer_2"),
        "the refusal must name both ids: {refusal}"
    );
    assert_eq!(queue.queued(), 0);
}

/// Both arguments are required, and neither may be blank — a hand-off with
/// no instruction is a turn run on nothing.
#[tokio::test]
async fn the_teammate_tool_requires_both_arguments() {
    let company = CompanyId::new("acme");
    let queue = DelegationQueue::default();
    let _claim = queue.claim();
    let tool = member_teammate_tool(peers_record(&company), &queue);
    assert!(tool.execute(json!({ "teammate": "editor" })).await.is_err());
    assert!(
        tool.execute(json!({ "instruction": "do it" }))
            .await
            .is_err()
    );
    assert!(
        tool.execute(json!({ "teammate": "  ", "instruction": "do it" }))
            .await
            .is_err()
    );
    assert_eq!(queue.queued(), 0);
}

/// Issue #272: the observed failure — the orchestrator handed work to
/// `writer`, which is a teammate rather than a desk. Nothing may be queued,
/// and the refusal must carry the real desk ids so the model can correct
/// itself in the same turn.
#[tokio::test]
async fn delegate_to_desk_tool_refuses_a_desk_that_does_not_exist() {
    let queue = DelegationQueue::default();
    let tool = desk_tool(desks_record(&CompanyId::new("acme")), &queue);
    let result = tool
        .execute(json!({ "desk": "writer", "instruction": "draft the release note" }))
        .await
        .expect("execute");
    assert!(result.is_error, "an invented desk must be refused");
    let text = result.output_for_llm(true);
    assert!(text.contains("strategy"), "valid ids must be named: {text}");
    assert!(
        text.contains("teammate"),
        "a teammate-as-desk target must be named as such: {text}"
    );
    assert_eq!(
        queue.queued(),
        0,
        "a refused target must not survive as a queued hand-off"
    );
    assert_eq!(
        queue.drain_refusals(MAX_DELEGATIONS_PER_TURN),
        vec!["writer".to_string()],
        "the drain must be able to report the attempt on the card"
    );
}

/// A desk that exists but has nobody on the roster can never run a turn, so
/// the hand-off is refused rather than queued into a drain that cannot
/// deliver it.
#[tokio::test]
async fn delegate_to_desk_tool_refuses_a_desk_with_no_roster_lead() {
    let queue = DelegationQueue::default();
    let tool = desk_tool(desks_record(&CompanyId::new("acme")), &queue);
    let result = tool
        .execute(json!({ "desk": "archive", "instruction": "file it" }))
        .await
        .expect("execute");
    assert!(result.is_error, "a leadless desk must be refused");
    let text = result.output_for_llm(true);
    assert!(
        text.contains("no member on the roster"),
        "the refusal must name the cause: {text}"
    );
    assert!(
        text.contains("strategy"),
        "a desk that CAN take work must be offered: {text}"
    );
    assert_eq!(queue.queued(), 0);
}

/// Fail-open: with no record to read, delegation behaves exactly as it did
/// before grounding existed. A store gap must not take delegation offline.
#[tokio::test]
async fn delegate_to_desk_tool_queues_ungrounded_when_no_record_is_readable() {
    let queue = DelegationQueue::default();
    // Claimed (issue #453): "fail open" is about the *desk grounding*, and
    // this pins that an unreadable record still queues. Whether anything
    // drains is a separate question with its own refusal.
    let _claim = queue.claim();
    let tool = DelegateToDeskTool::new(
        queue.clone(),
        CompanyId::new("acme"),
        Arc::new(MemStore::default()) as Arc<dyn CompanyStore>,
    );
    let result = tool
        .execute(json!({ "desk": "whatever", "instruction": "do it" }))
        .await
        .expect("execute");
    assert!(!result.is_error);
    assert_eq!(queue.queued(), 1);
}

/// **Issue #348 review.** The recent-activity tail is ten slots wide, and a
/// discussion (#335) is an operator-driven writer into the same journal the
/// tail reads. A row per post would let one afternoon's thread on one card
/// push every dispatch, reply and approval out of the orchestrator's only
/// view of what the company has been doing — and replace them with rows it
/// cannot act on, since no agent participates in a discussion.
///
/// So: posts never hold a slot, the run events survive a thread that
/// outnumbers them, and the fact that people are talking is still reported —
/// as one folded count, with no message text (the same no-quoting rule
/// `summarize_event`'s arm carries).
#[tokio::test]
async fn discussion_posts_fold_to_one_line_instead_of_evicting_the_activity_tail() {
    use crate::ports::types::StoredEvent;
    use futures::stream::{self, BoxStream};

    /// A log that replays a fixed history.
    struct FixedLog(Vec<StoredEvent>);

    #[async_trait]
    impl EventLog for FixedLog {
        async fn append(&self, _id: &CompanyId, _event: CompanyEvent) -> crate::Result<EventSeq> {
            unreachable!("the insight surface only reads")
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            seq: EventSeq,
            limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            Ok(self
                .0
                .iter()
                .filter(|e| e.seq.value() >= seq.value())
                .take(limit)
                .cloned()
                .collect())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    let company = CompanyId::new("acme");
    let mut history = vec![StoredEvent {
        seq: EventSeq::new(0),
        company: company.clone(),
        event: CompanyEvent::TaskDispatched {
            task_id: "t-1".to_string(),
            run_id: None,
            origin_chat_id: None,
            origin_parent: None,
        },
        at_millis: 1,
    }];
    // Twenty posts — twice the tail — on the one card, as an afternoon of
    // back-and-forth actually looks.
    for n in 0..20u64 {
        history.push(StoredEvent {
            seq: EventSeq::new(n + 1),
            company: company.clone(),
            event: CompanyEvent::TaskDiscussionPosted {
                task_id: "t-1".to_string(),
                text: format!("ping the vendor again ({n})"),
                by: None,
            },
            at_millis: 2 + n,
        });
    }
    history.push(StoredEvent {
        seq: EventSeq::new(21),
        company: company.clone(),
        event: CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".to_string(),
            desk: "eng".to_string(),
            output: "shipped".to_string(),
            column: "done".to_string(),
            artifact_ids: Vec::new(),
            origin_chat_id: None,
            origin_parent: None,
        },
        at_millis: 30,
    });

    let log: Arc<dyn EventLog> = Arc::new(FixedLog(history));
    let tool = QueryCompanyTool::new(company, None, Some(log), None, None, None);
    let out = tool
        .execute(json!({}))
        .await
        .expect("execute")
        .output_for_llm(true);

    // Both run events survive the thread that buried them.
    assert!(out.contains("task dispatched"), "dispatch evicted: {out}");
    assert!(out.contains("task completed"), "completion evicted: {out}");
    // One folded line, not twenty rows — and no message text anywhere.
    assert!(out.contains("20 discussion posts"), "{out}");
    assert!(!out.contains("ping the vendor"), "post text quoted: {out}");
    assert_eq!(
        out.matches("discussion post").count(),
        1,
        "a post must not hold a slot of its own: {out}"
    );
}

/// Issue #420: the recent-activity tail keeps only [`RECENT_EVENTS`] rows,
/// and it used to drop everything older in silence — a full log read as
/// complete, the same silent-cut class the facts section one block down
/// already announces. The tail now names how many rows fell off the far end
/// (in the markdown) and reports the count (in the JSON summary). Discussion
/// posts pushed past the tail are dropped rows too, so they count toward it
/// rather than folding into their own line.
#[tokio::test]
async fn query_company_announces_the_dropped_event_tail() {
    use crate::ports::types::StoredEvent;
    use futures::stream::{self, BoxStream};

    /// A log that replays a fixed history.
    struct FixedLog(Vec<StoredEvent>);

    #[async_trait]
    impl EventLog for FixedLog {
        async fn append(&self, _id: &CompanyId, _event: CompanyEvent) -> crate::Result<EventSeq> {
            unreachable!("the insight surface only reads")
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            seq: EventSeq,
            limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            Ok(self
                .0
                .iter()
                .filter(|e| e.seq.value() >= seq.value())
                .take(limit)
                .cloned()
                .collect())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    let company = CompanyId::new("acme");

    // A distinct, non-discussion event so every row occupies a tail slot.
    let dispatch = |seq: u64| StoredEvent {
        seq: EventSeq::new(seq),
        company: company.clone(),
        event: CompanyEvent::TaskDispatched {
            task_id: format!("t-{seq}"),
            run_id: None,
            origin_chat_id: None,
            origin_parent: None,
        },
        at_millis: seq + 1,
    };

    // (a) Five more row-events than the tail is wide: the five oldest fall
    // off, the notice sits at the top, and the JSON summary counts them.
    let over: Vec<StoredEvent> = (0..(RECENT_EVENTS as u64 + 5)).map(dispatch).collect();
    let log: Arc<dyn EventLog> = Arc::new(FixedLog(over));
    let tool = QueryCompanyTool::new(company.clone(), None, Some(log), None, None, None);
    let result = tool.execute(json!({})).await.expect("execute");
    let md = result.output_for_llm(true);
    let activity = md
        .split("## Recent activity\n")
        .nth(1)
        .expect("recent activity section");
    assert!(
        activity.starts_with("- […5 earlier event(s) not shown]"),
        "the dropped tail must be announced at the top: {md}"
    );
    assert!(
        result
            .output_for_llm(false)
            .contains("\"events_not_shown\": 5"),
        "the JSON summary must count the drop: {}",
        result.output_for_llm(false)
    );

    // (b) Exactly the tail width: nothing was dropped, so nothing is said.
    let exact: Vec<StoredEvent> = (0..RECENT_EVENTS as u64).map(dispatch).collect();
    let log: Arc<dyn EventLog> = Arc::new(FixedLog(exact));
    let result = QueryCompanyTool::new(company.clone(), None, Some(log), None, None, None)
        .execute(json!({}))
        .await
        .expect("execute");
    assert!(
        !result
            .output_for_llm(true)
            .contains("earlier event(s) not shown"),
        "a complete tail must stay silent: {}",
        result.output_for_llm(true)
    );
    assert!(
        result
            .output_for_llm(false)
            .contains("\"events_not_shown\": 0"),
        "a complete tail reports zero dropped: {}",
        result.output_for_llm(false)
    );

    // (c) Discussion posts older than the tail are dropped rows: they count
    // toward the drop, not toward the fold line. Three posts (oldest) then
    // enough dispatches to fill the tail — the posts never get visited.
    let mut mixed: Vec<StoredEvent> = Vec::new();
    for seq in 0..3u64 {
        mixed.push(StoredEvent {
            seq: EventSeq::new(seq),
            company: company.clone(),
            event: CompanyEvent::TaskDiscussionPosted {
                task_id: "t-1".to_string(),
                text: format!("older chatter {seq}"),
                by: None,
            },
            at_millis: seq + 1,
        });
    }
    for seq in 3..(RECENT_EVENTS as u64 + 5) {
        mixed.push(dispatch(seq));
    }
    // total = 3 posts + (RECENT_EVENTS + 2) dispatches; the tail holds
    // RECENT_EVENTS dispatches, so 2 dispatches + 3 posts = 5 fall off.
    let log: Arc<dyn EventLog> = Arc::new(FixedLog(mixed));
    let result = QueryCompanyTool::new(company.clone(), None, Some(log), None, None, None)
        .execute(json!({}))
        .await
        .expect("execute");
    let md = result.output_for_llm(true);
    assert!(
        md.contains("- […5 earlier event(s) not shown]"),
        "dropped discussion posts must count toward the tail drop: {md}"
    );
    assert!(
        !md.contains("discussion post"),
        "an unvisited post must not also fold into its own line: {md}"
    );
    assert!(
        result
            .output_for_llm(false)
            .contains("\"events_not_shown\": 5"),
        "{}",
        result.output_for_llm(false)
    );
}

/// Issue #410, point 4 (audit the same silent-cut class elsewhere): the
/// fact list is capped at [`FACT_LIMIT`], and it used to be capped in
/// silence. A company past twenty facts handed the orchestrator a partial
/// memory that read as complete, so "we have no record of that" was a
/// conclusion it could reach from a truncated list. The cut now says it
/// happened and names the argument that narrows it.
#[tokio::test]
async fn query_company_says_when_the_fact_list_was_cut() {
    use crate::ports::FactStore;
    use crate::ports::facts::{FactKind, FactRecord};

    struct ManyFacts(usize);
    #[async_trait]
    impl FactStore for ManyFacts {
        async fn list(
            &self,
            _company: &CompanyId,
            _query: Option<&str>,
            _kind: Option<FactKind>,
        ) -> crate::Result<Vec<FactRecord>> {
            Ok((0..self.0)
                .map(|i| FactRecord {
                    id: format!("f-{i}"),
                    kind: FactKind::Fact,
                    title: format!("Fact {i}"),
                    body: format!("Body {i}"),
                    source: "ceo".to_string(),
                    updated_at_millis: i as u64,
                })
                .collect())
        }
        async fn upsert(&self, _c: &CompanyId, _f: &FactRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn delete(&self, _c: &CompanyId, _id: &str) -> crate::Result<bool> {
            Ok(false)
        }
    }

    // Exactly at the cap: complete, so no notice.
    let exact: Arc<dyn FactStore> = Arc::new(ManyFacts(FACT_LIMIT));
    let out = QueryCompanyTool::new(CompanyId::new("acme"), Some(exact), None, None, None, None)
        .execute(json!({}))
        .await
        .expect("execute")
        .output_for_llm(true);
    assert!(!out.contains("TRUNCATED"), "nothing was cut: {out}");

    // Past the cap: the cut is announced, counted, and points at `query`.
    let many: Arc<dyn FactStore> = Arc::new(ManyFacts(FACT_LIMIT + 7));
    let out = QueryCompanyTool::new(CompanyId::new("acme"), Some(many), None, None, None, None)
        .execute(json!({}))
        .await
        .expect("execute")
        .output_for_llm(true);
    assert!(
        out.contains("TRUNCATED"),
        "the cut must be announced: {out}"
    );
    assert!(out.contains("7 more fact(s) not shown"), "{out}");
    assert!(out.contains("query_company"), "{out}");
}

/// Issue #420, the residual: the whole insight document is handed to the
/// model through the harness tool-result path, which hard-cuts anything past
/// its byte budget — blindly. A facts list long enough would carry that cut
/// into the sections below it, dropping the facts `[TRUNCATED]` marker and
/// the Desks list `delegate_to_desk` reads. So each fact body is capped and
/// the facts section is bounded in bytes; the marker and every later section
/// stay inside the outer budget. Cutting a body counts characters, never
/// bytes, so a multibyte body cannot panic mid-codepoint.
#[tokio::test]
async fn query_company_bounds_the_insight_document_size() {
    use crate::ports::FactStore;
    use crate::ports::facts::{FactKind, FactRecord};

    struct Facts(Vec<FactRecord>);
    #[async_trait]
    impl FactStore for Facts {
        async fn list(
            &self,
            _c: &CompanyId,
            _q: Option<&str>,
            _k: Option<FactKind>,
        ) -> crate::Result<Vec<FactRecord>> {
            Ok(self.0.clone())
        }
        async fn upsert(&self, _c: &CompanyId, _f: &FactRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn delete(&self, _c: &CompanyId, _id: &str) -> crate::Result<bool> {
            Ok(false)
        }
    }

    let mk = |i: usize, body: String| FactRecord {
        id: format!("f-{i}"),
        kind: FactKind::Fact,
        title: format!("Fact {i}"),
        body,
        source: "ceo".to_string(),
        updated_at_millis: i as u64,
    };
    let render = |facts: Vec<FactRecord>| async move {
        let store: Arc<dyn FactStore> = Arc::new(Facts(facts));
        QueryCompanyTool::new(CompanyId::new("acme"), Some(store), None, None, None, None)
            .execute(json!({}))
            .await
            .expect("execute")
            .output_for_llm(true)
    };

    // (e) A single multi-KB multibyte body: cut on a char boundary, marked
    // with an ellipsis, exactly the cap wide, and no panic.
    let out = render(vec![mk(0, "é".repeat(5_000))]).await;
    let line = out
        .lines()
        .find(|l| l.starts_with("- **Fact 0**: "))
        .expect("fact line");
    let body = line.strip_prefix("- **Fact 0**: ").unwrap();
    assert!(body.ends_with('…'), "a cut body is marked: {body:?}");
    assert_eq!(
        body.chars().count(),
        MAX_FACT_BODY_CHARS,
        "the body is cut to exactly the cap"
    );
    assert!(
        body.chars().take(MAX_FACT_BODY_CHARS - 1).all(|c| c == 'é'),
        "the cut landed on a codepoint boundary, not inside one"
    );

    // (f) Enough capped bodies to blow the section byte budget. The count
    // reflects the budget cut, not merely FACT_LIMIT, and the marker plus
    // every section below Facts survives the outer tool-result cut.
    let heavy: Vec<FactRecord> = (0..FACT_LIMIT)
        .map(|i| mk(i, "é".repeat(MAX_FACT_BODY_CHARS)))
        .collect();
    let out = render(heavy).await;
    let shown = out.matches("- **Fact ").count();
    assert!(
        (1..FACT_LIMIT).contains(&shown),
        "the byte budget must cut before FACT_LIMIT yet keep at least one: shown={shown}"
    );
    assert!(
        out.contains(&format!("{} more fact(s) not shown", FACT_LIMIT - shown)),
        "the marker counts the budget cut: {out}"
    );
    for header in [
        "[TRUNCATED",
        "## Recent activity",
        "## Saved workflows",
        "## Team",
        "## Desks",
    ] {
        assert!(
            out.contains(header),
            "the facts cut must not carry the outer budget into `{header}`: {out}"
        );
    }

    // (g) A small document is byte-for-byte the pre-guard behavior: bodies
    // under the cap render verbatim and nothing is announced.
    let out = render(vec![
        mk(0, "Body 0".to_string()),
        mk(1, "Body 1".to_string()),
    ])
    .await;
    assert!(
        out.contains("## Facts\n- **Fact 0**: Body 0\n- **Fact 1**: Body 1\n"),
        "the small-document path is unchanged: {out}"
    );
    assert!(!out.contains("TRUNCATED"), "nothing was cut: {out}");
    assert!(!out.contains('…'), "nothing was truncated: {out}");
}
