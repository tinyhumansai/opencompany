//! `built_in`'s own inline tests, part 10 of 10. Split out of the
//! single inline `mod tests` block because it exceeded the 750-line file
//! limit; grouped in the original file's order, not by topic (the block
//! covered dozens of unrelated issues with no existing topical boundaries).
//! Shared setup lives in [`super::built_in_test_fixtures`] and
//! [`super::built_in_test_fixtures_2`].

use super::built_in_test_fixtures::*;
use super::built_in_test_fixtures_2::*;
use super::*;
use crate::company::CompanyManifest;

/// An **overlay** teammate — one added from the console, with no manifest
/// row — can be capped through the same override, and is refused when it has
/// spent it. Before #343 an overlay teammate was unconditionally uncapped
/// ("overlay teammates are uncapped in v1"), so this is a capability that did
/// not exist rather than a behaviour that changed.
#[tokio::test]
async fn an_overlay_teammate_can_be_capped_from_the_console() {
    use crate::ports::types::{Actor, ActorKind, BudgetOverride};

    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(MockContext::default());
    let meter = Arc::new(RecordingMeter::default());

    let mut rec = record();
    rec.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "growth".into(),
        name: "Jamie".into(),
        role: "Growth Lead".into(),
        description: None,
        tools: None,
        model: None,
        harness: None,
    });
    let live_store = Arc::new(LiveStore::default());
    live_store.save(&rec).await.unwrap();

    let mut deps = deps_with_plan(
        dir.path(),
        context.clone(),
        Some(meter.clone() as Arc<dyn UsageMeter>),
        None,
    );
    deps.store = live_store.clone();
    let pool = HarnessPool::new();

    // Uncapped to begin with: it answers.
    pool.ensure(&rec, &deps).await.expect("ensure");
    let reply = pool
        .run(
            &rec.id,
            "growth",
            "hello-marker",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("an uncapped overlay teammate answers")
        .reply;
    assert!(reply.contains("hello-marker"), "got {reply:?}");

    // The operator caps it at $1 and it has already spent $2.
    meter
        .record(
            &rec.id,
            &spend_sample("growth", 2.00, crate::ports::now_millis()),
        )
        .await
        .unwrap();
    let mut capped = rec.clone();
    capped.overlay_budgets = vec![BudgetOverride {
        agent_id: "growth".to_string(),
        budget_usd_daily: Some(1.0),
        set_by: Actor {
            kind: ActorKind::User,
            id: "user-admin".to_string(),
        },
        at_millis: crate::ports::now_millis(),
    }];
    live_store.save(&capped).await.unwrap();

    pool.ensure(&rec, &deps).await.expect("ensure again");
    let refused = pool
        .run(
            &rec.id,
            "growth",
            "should-not-echo",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("a refusal is a benign outcome")
        .reply;
    assert_eq!(
        refused,
        agent_budget_exhausted_notice("growth", 1.0),
        "a console-added teammate is capped by the same gate as a manifest one"
    );
}

/// A declared `budget_usd_daily` that cannot be read refuses dispatch to
/// that teammate — the same rule as the total ceiling, at the layer that
/// carries the money.
///
/// The cap here is a generous `$50`, so a readable meter would admit both
/// turns below; the refusals can only come from the spend read failing.
/// The uncapped colleague keeps working either way: the rule bites the
/// scope that declared a bound, not the company.
#[tokio::test]
async fn run_refuses_a_capped_teammate_when_spend_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(MockContext::default());
    let manifest: CompanyManifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction."
budget_usd_daily = 50.0

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds the product."
"#,
    )
    .expect("valid manifest");
    let rec = CompanyRecord {
        manifest,
        ..record()
    };

    // A meter that errors: a transient read fault.
    let failing = deps_with_plan(
        dir.path(),
        context.clone(),
        Some(Arc::new(FailingMeter) as Arc<dyn UsageMeter>),
        None,
    );
    let pool = HarnessPool::new();
    pool.ensure(&rec, &failing).await.expect("ensure");
    let reply = pool
        .run(
            &rec.id,
            "ceo",
            "hello-marker",
            &failing,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("a refusal is a benign outcome, not a hard error")
        .reply;
    assert!(
        !reply.contains("hello-marker"),
        "no model call may run against an unreadable cap: {reply:?}"
    );
    assert_eq!(
        reply,
        unmeasurable_agent_budget_notice(
            "ceo",
            50.0,
            &SpendReadFault::QueryFailed(OpenCompanyError::Store("meter unavailable".into()))
        ),
        "the refusal names the teammate, its cap, and that the read may clear"
    );
    let failed_read_reply = reply;

    // The uncapped colleague declared no bound, so there is nothing to fail
    // closed on and it keeps working through the same broken meter.
    let ok = pool
        .run(
            &rec.id,
            "engineer",
            "hello-marker",
            &failing,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("an uncapped teammate keeps working")
        .reply;
    assert!(
        ok.contains("hello-marker"),
        "an uncapped teammate is not gated by a broken meter: {ok:?}"
    );

    // No meter at all: the cap is permanently unenforceable on this host —
    // a deployment fault rather than a transient read.
    let no_meter = deps_with_plan(dir.path(), context.clone(), None, None);
    let pool = HarnessPool::new();
    pool.ensure(&rec, &no_meter).await.expect("ensure");
    let reply = pool
        .run(
            &rec.id,
            "ceo",
            "hello-marker",
            &no_meter,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("a refusal is a benign outcome, not a hard error")
        .reply;
    assert!(
        !reply.contains("hello-marker"),
        "no model call may run against an unmeasurable cap: {reply:?}"
    );
    assert_eq!(
        reply,
        unmeasurable_agent_budget_notice("ceo", 50.0, &SpendReadFault::NoMeter),
        "an absent meter is reported as the deployment fault it is, and the reply says what \
         the operator has to change"
    );
    assert_ne!(
        reply, failed_read_reply,
        "the two faults are not the same fault and must not read as one"
    );
}

/// The cap is the UTC calendar day: yesterday's $9 does not refuse today's
/// first turn. Depends on `RecordingMeter` honouring `since_millis`.
#[tokio::test]
async fn a_yesterday_stamped_spend_does_not_refuse_todays_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(MockContext::default());
    let meter = Arc::new(RecordingMeter::default());
    let rec = capped_record();

    let yesterday =
        crate::metering::utc_day_start_millis(crate::ports::now_millis()).saturating_sub(1);
    meter
        .record(&rec.id, &spend_sample("ceo", 9.00, yesterday))
        .await
        .unwrap();

    let deps = deps_with_plan(
        dir.path(),
        context.clone(),
        Some(meter.clone() as Arc<dyn UsageMeter>),
        None,
    );
    let pool = HarnessPool::new();
    pool.ensure(&rec, &deps).await.expect("ensure");

    let reply = pool
        .run(
            &rec.id,
            "ceo",
            "hello-marker",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("a new day admits the turn")
        .reply;
    assert!(
        reply.contains("hello-marker"),
        "the cap resets at 00:00Z; yesterday's spend is spent: {reply:?}"
    );
}

/// **The mechanism issue #443 asks for.** Every tool this crate can put in
/// front of an agent must be classified in
/// [`crate::policy::consequence`], or this fails.
///
/// Three families had needed the same carve-out before it, each added after
/// somebody hit it, and what the gate did with the ones nobody hit was
/// silent: the tool simply started asking for permission, and whoever
/// noticed was an operator wondering why a read needed approving. That is
/// how `mcp_list_servers` — which the agent persona *instructs* every agent
/// to call — came to cost an approval, and how `file_read`, `glob` and
/// `grep` came to park with nobody reporting it.
///
/// A tool declaring its own consequence to the gate at call time would be
/// better, and is not reachable: openhuman's `ToolPolicy` surface hands the
/// bridge a name and arguments, never the tool. So the declaration is
/// checked against the live belt here instead — the issue's own stated
/// fallback, "exhaustive by construction rather than by memory".
///
/// Feature-aware by construction: it enumerates whatever this build wires,
/// so a family behind a cargo feature is covered by the lane that enables
/// it rather than by a `cfg` branch that has to be kept in step.
#[test]
fn every_registered_tool_is_declared() {
    let declared: std::collections::BTreeSet<&str> =
        crate::policy::consequence::declared_tools().collect();
    let mut live: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (grants, orchestrator, everything) in [
        (&["*"][..], false, false),
        (&["*"][..], true, false),
        (&["*"][..], false, true),
        (&["*"][..], true, true),
        // `*` deliberately does not reach MCP (`grants_cover_server` treats
        // it as an explicit opt-in), so a family grant is what wires the
        // three bridge tools here — the same `mcp:*` a company would use.
        (
            &["workspace", "search", "media", "composio", "mcp:*"][..],
            false,
            true,
        ),
    ] {
        live.extend(belt(grants, orchestrator, everything));
    }
    // A vacuity guard with teeth. `!live.is_empty()` would not notice the
    // belt quietly narrowing to the tools nobody was worried about, and
    // three of the four names below are the ones the issues are about.
    for expected in [
        "shell",
        "workspace_write",
        "file_read",
        "describe_skill",
        #[cfg(feature = "mcp")]
        "mcp_list_servers",
        #[cfg(feature = "mcp")]
        "mcp_call_tool",
    ] {
        assert!(
            live.contains(expected),
            "the belt builder stopped wiring `{expected}`, so this check has \
             narrowed without anyone deciding to narrow it: {live:?}"
        );
    }
    let undeclared: Vec<&String> = live
        .iter()
        .filter(|name| !declared.contains(name.as_str()))
        .collect();
    assert!(
        undeclared.is_empty(),
        "these tools are wired onto a live agent but nobody has said what they can \
         reach, so the gate is guessing from their names and they cannot be granted \
         standing: {undeclared:?}. Add them to `crate::policy::consequence::DECLARED`."
    );
}

/// The one-directional cross-check on the declaration.
///
/// A tool's own `permission_level()` is NOT trustworthy as the authority —
/// it defaults to `ReadOnly`, and upstream tools that plainly mutate
/// (`git_operations`, `memory_store`) never override it, so believing a
/// `ReadOnly` claim would wave a write straight through the gate. But the
/// claims in the *other* direction are deliberate: nothing declares itself
/// `Execute` or `Dangerous` by accident. So those are checked, and a
/// `ReadOnly` claim is ignored.
#[test]
fn nothing_that_declares_itself_executable_is_internal_or_grantable() {
    use oh::tools::traits::PermissionLevel;
    let dir = tempfile::tempdir().expect("tempdir");
    let deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
    let manifest_agent = ManifestAgent {
        provider: None,
        global: false,
        id: "desk".to_string(),
        role: "Desk Lead".to_string(),
        name: None,
        description: None,
        tier: None,
        harness: None,
        tools: None,
        delegates_to: Vec::new(),
        context: None,
        budget_usd_daily: None,
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
        model: None,
    };
    let agent = build::build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        ApprovalPolicy::new(&Policy::default(), None),
        &deps,
        &["*".to_string()],
        &[],
        &[],
        None,
        true,
        /* speech_enabled */ false,
    )
    .expect("agent builds");
    let args = serde_json::json!({});
    let mut checked = 0;
    for tool in agent.tools() {
        if !matches!(
            tool.permission_level(),
            PermissionLevel::Execute | PermissionLevel::Dangerous
        ) {
            continue;
        }
        checked += 1;
        let verdict = crate::policy::consequence_of(tool.name(), &args);
        assert!(
            verdict.reach.denied_under_readonly(),
            "`{}` declares itself executable but a read-only desk would allow it",
            tool.name()
        );
        assert!(
            !verdict.standing.is_grantable(),
            "`{}` declares itself executable and must not be grantable",
            tool.name()
        );
    }
    assert!(checked > 0, "no executable tool was on the belt to check");
}

#[cfg(feature = "chargebee")]
#[tokio::test]
async fn chargebee_resolves_only_for_a_company_that_grants_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let secrets = Arc::new(BillingSecrets::default());
    secrets
        .set(
            &CompanyId::new("acme"),
            crate::chargebee::types::SITE_SECRET,
            crate::ports::types::SecretValue("acme-test".into()),
        )
        .await
        .expect("seed");
    secrets
        .set(
            &CompanyId::new("acme"),
            crate::chargebee::types::API_KEY_SECRET,
            crate::ports::types::SecretValue("cb_key".into()),
        )
        .await
        .expect("seed");
    let deps = billing_deps(dir.path(), secrets);
    let pool = HarnessPool::new();

    // Granted and configured: the credential resolves.
    let granted = pool
        .resolve_chargebee(&record_granting(&["chargebee"]), &deps)
        .await
        .expect("a granted, configured company resolves");
    assert_eq!(granted.site(), "acme-test");

    // Same credentials, no grant. The store is untouched — the gate is the
    // manifest, so a company that never opted in gets no tools however well
    // configured the host happens to be.
    assert!(
        pool.resolve_chargebee(&record_granting(&[]), &deps)
            .await
            .is_none(),
        "an ungranted company must resolve nothing"
    );

    // And a wildcard is not a grant: these tools send invoices to real
    // people, so they are opted into by name rather than riding in on the
    // `*` somebody set for file and shell tools.
    assert!(
        pool.resolve_chargebee(&record_granting(&["*"]), &deps)
            .await
            .is_none(),
        "a catch-all grant must not confer chargebee"
    );
}

#[cfg(feature = "chargebee")]
#[tokio::test]
async fn a_chargebee_store_hiccup_keeps_the_last_known_connection() {
    // The distinction this pins: absence wires no tools, but a READ FAILURE
    // keeps whatever was already resolved. Collapsing the two would drop a
    // working company's billing tools mid-conversation on one bad read, and
    // silently — the agent would simply stop being able to invoice.
    let dir = tempfile::tempdir().expect("tempdir");
    let secrets = Arc::new(BillingSecrets {
        fail: true,
        ..Default::default()
    });
    let mut deps = billing_deps(dir.path(), secrets);
    let last_known = crate::harness::chargebee::TenantChargebee::resolve(
        &(Arc::new(BillingSecrets {
            map: StdMutex::new(
                [
                    (
                        crate::chargebee::types::SITE_SECRET.to_string(),
                        "acme-test".to_string(),
                    ),
                    (
                        crate::chargebee::types::API_KEY_SECRET.to_string(),
                        "cb_key".to_string(),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
            fail: false,
        }) as Arc<dyn SecretStore>),
        &CompanyId::new("acme"),
    )
    .await
    .expect("the seeded store reads")
    .expect("both halves present");
    deps.chargebee = Some(last_known);

    let kept = pool_resolve_chargebee(&deps).await;
    assert_eq!(
        kept.map(|c| c.site().to_string()).as_deref(),
        Some("acme-test"),
        "a transient read failure must not disconnect a working integration"
    );
}

#[cfg(feature = "paypal")]
#[tokio::test]
async fn paypal_resolves_only_for_a_company_that_grants_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let secrets = Arc::new(BillingSecrets::default());
    for (key, value) in [
        (crate::company::paypal::CLIENT_ID_SECRET, "AY_id"),
        (crate::company::paypal::CLIENT_SECRET_SECRET, "EL_secret"),
    ] {
        secrets
            .set(
                &CompanyId::new("acme"),
                key,
                crate::ports::types::SecretValue(value.into()),
            )
            .await
            .expect("seed");
    }
    let deps = billing_deps(dir.path(), secrets);
    let pool = HarnessPool::new();

    assert!(
        pool.resolve_paypal(&record_granting(&["paypal"]), &deps)
            .await
            .is_some(),
        "a granted, configured company resolves"
    );
    assert!(
        pool.resolve_paypal(&record_granting(&[]), &deps)
            .await
            .is_none(),
        "an ungranted company must resolve nothing"
    );
    assert!(
        pool.resolve_paypal(&record_granting(&["*"]), &deps)
            .await
            .is_none(),
        "a catch-all grant must not confer paypal"
    );
}

#[cfg(feature = "paypal")]
#[tokio::test]
async fn a_paypal_grant_with_no_credential_wires_nothing_rather_than_failing() {
    // Fail closed: a manifest that grants `paypal` on a host where nobody
    // has saved a credential must wire no tools, not tools that fail on
    // first use — an agent that HAS a wallet tool tells the operator the
    // balance is unavailable, rather than that it cannot read wallets.
    let dir = tempfile::tempdir().expect("tempdir");
    let deps = billing_deps(dir.path(), Arc::new(BillingSecrets::default()));
    assert!(
        HarnessPool::new()
            .resolve_paypal(&record_granting(&["paypal"]), &deps)
            .await
            .is_none()
    );
}

/// Issue #2369: the three destinations a dispatched card's turn can stream to.
///
/// Pinned on [`dispatch_live_stream`] directly, because the interesting part
/// is the decision and not the turn around it. A dispatch runs for minutes, so
/// getting this wrong is not a cosmetic bug in either direction: streaming a
/// board-created card publishes an agent's frames into whatever thread is open
/// (#125), and refusing to stream one raised in a thread is the silence this
/// issue is about.
///
/// The thread root is deliberately *absent* from every `On` here. Issue #1890
/// I split "which conversation is this turn in" from "where do its frames go",
/// so identity rides the `ChatTarget` and only the desk reaches the stream —
/// which is why the channel-level and threaded cases produce the same variant.
#[test]
fn a_dispatch_streams_to_its_origin_and_a_board_card_streams_nowhere() {
    use crate::ports::types::EventSeq;
    use crate::runtime::delegation::ChatTarget;

    let in_thread = ChatTarget::dispatched_from(Some("strategy"), Some(EventSeq::new(41)));
    assert!(
        matches!(
            dispatch_live_stream(&in_thread),
            LiveStream::On {
                chat_id: Some("strategy")
            }
        ),
        "a card raised in a thread streams to that desk"
    );
    assert_eq!(
        in_thread.thread_root,
        Some(EventSeq::new(41)),
        "the thread stays on the target, which is what #1890 I separated"
    );

    let in_channel = ChatTarget::dispatched_from(Some("strategy"), None);
    assert!(
        matches!(
            dispatch_live_stream(&in_channel),
            LiveStream::On {
                chat_id: Some("strategy")
            }
        ),
        "a card raised at channel level streams to the desk with no thread"
    );
    assert!(in_channel.thread_root.is_none());

    let on_board = ChatTarget::dispatched_from(None, None);
    assert!(
        matches!(dispatch_live_stream(&on_board), LiveStream::Off),
        "a board-created card names no conversation, so it publishes nothing"
    );

    // None of the three may seed history: a dispatched turn is given its
    // instruction, not the thread's backlog (#1840).
    for target in [&in_thread, &in_channel, &on_board] {
        assert!(
            !target.history_seed,
            "a dispatched turn is never history-seeded"
        );
    }
}
