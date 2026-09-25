//! `built_in`'s own inline tests, part 3 of 10. Split out of the
//! single inline `mod tests` block because it exceeded the 750-line file
//! limit; grouped in the original file's order, not by topic (the block
//! covered dozens of unrelated issues with no existing topical boundaries).
//! Shared setup lives in [`super::built_in_test_fixtures`] and
//! [`super::built_in_test_fixtures_2`].

use super::built_in_test_fixtures::*;
use super::built_in_test_fixtures_2::*;
use super::*;
use crate::ports::types::ContextChunk;
use tinyinference::model::ModelRequest;

/// Issue #416 — a confined turn reaches the company's memory neither on the
/// way in nor on the way out.
///
/// The control half is what makes this a test rather than an assertion of
/// absence: the SAME message on the ordinary roster path pulls the seeded
/// chunk into the prompt (the mock provider echoes what it was sent, so the
/// injection is visible in the reply), and writes the turn back. The
/// confined path does neither, from the same store, in the same test.
#[tokio::test]
async fn a_confined_turn_neither_reads_nor_writes_company_memory() {
    let context = Arc::new(MockContext::default());
    let mut fx = fixture();
    fx.deps.context = context.clone();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    // A prior outcome sitting in the company's memory. The mock store
    // matches a chunk whose BODY contains the query, and retrieve→inject
    // queries with the whole message — so a body built around the message is
    // what a hit looks like here.
    let question = "why did it fail";
    context
        .put(
            &rec.id,
            ContextChunk {
                label: "prior/outcome".into(),
                body: format!("SECRET-PAYROLL-REVIEW: {question} on Monday"),
            },
        )
        .await
        .expect("seed the company's memory");
    let seeded = context.chunks.lock().unwrap().len();

    // Control: the ordinary path injects the hit and writes the turn back.
    let ordinary = pool
        .run(
            &rec.id,
            "ceo",
            question,
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("the ordinary turn runs")
        .reply;
    assert!(
        ordinary.contains("SECRET-PAYROLL-REVIEW"),
        "the retrieve→inject step must be live for this test to mean anything: {ordinary}"
    );
    assert!(
        context.chunks.lock().unwrap().len() > seeded,
        "the ordinary path writes its outcome back to company memory"
    );

    let before_confined = context.chunks.lock().unwrap().len();
    let confined = pool
        .run_confined(
            &rec.id,
            "Acme",
            question,
            &fx.deps,
            Some("workflow-copilot:weekly_report"),
            &confine::Confinement::workflow("weekly_report"),
        )
        .await
        .expect("the confined turn runs")
        .reply;

    assert!(
        confined.contains(question),
        "the confined turn still answers the question it was asked: {confined}"
    );
    assert!(
        !confined.contains("SECRET-PAYROLL-REVIEW"),
        "a confined turn must not be handed company memory: {confined}"
    );
    assert_eq!(
        context.chunks.lock().unwrap().len(),
        before_confined,
        "a confined turn must leave nothing behind for a later turn to retrieve"
    );
}

/// The confined agent is not on the roster, so nothing can address it: a
/// dispatch, a desk hand-off or a `chat` naming it is an unknown agent, the
/// same as any other name that is not a teammate.
#[tokio::test]
async fn the_confined_agent_is_not_addressable() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    let err = pool
        .run(
            &rec.id,
            confine::CONFINED_AGENT_ID,
            "hi",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect_err("the confined agent is not a roster agent");
    assert!(
        matches!(err, OpenCompanyError::InvalidRequest(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn unknown_agent_is_invalid_request() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    let err = pool
        .run(
            &rec.id,
            "nobody",
            "hi",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect_err("unknown agent rejected");
    assert!(
        matches!(err, OpenCompanyError::InvalidRequest(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn unknown_company_is_not_found() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let err = pool
        .run(
            &CompanyId::new("ghost"),
            "ceo",
            "hi",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect_err("unknown company rejected");
    assert!(
        matches!(err, OpenCompanyError::CompanyNotFound(_)),
        "{err:?}"
    );
}

/// The whole transition table, exhaustively: a broken volume must produce
/// one error line and then nothing, and a recovery must be announced once.
#[test]
fn workspace_report_is_edge_triggered() {
    let mut failing: HashSet<&str> = HashSet::new();

    // First failure speaks.
    assert_eq!(
        workspace_report(&mut failing, &"a", true),
        WorkspaceReport::Failed
    );
    // Every repeat is silent — this is the flood #449 is about.
    for _ in 0..100 {
        assert_eq!(
            workspace_report(&mut failing, &"a", true),
            WorkspaceReport::StillFailing
        );
    }
    // Recovery speaks exactly once.
    assert_eq!(
        workspace_report(&mut failing, &"a", false),
        WorkspaceReport::Recovered
    );
    assert_eq!(
        workspace_report(&mut failing, &"a", false),
        WorkspaceReport::StillHealthy
    );
    // A healthy agent that was never failing says nothing on its first
    // attempt either — a working workspace has never been worth a line.
    assert_eq!(
        workspace_report(&mut failing, &"never-failed", false),
        WorkspaceReport::StillHealthy
    );
    // And it can fail again later: the edge re-arms.
    assert_eq!(
        workspace_report(&mut failing, &"a", true),
        WorkspaceReport::Failed
    );

    assert!(
        WorkspaceReport::StillFailing.is_silent() && WorkspaceReport::StillHealthy.is_silent(),
        "only the repeats are silent"
    );
    assert!(
        !WorkspaceReport::Failed.is_silent() && !WorkspaceReport::Recovered.is_silent(),
        "both edges must be reported"
    );
}

/// Two agents interleaved: one failing, one healthy. Each key's edge is its
/// own — a second agent's failure must not be swallowed by the first's, and
/// a second agent's recovery must not clear the first's failure.
#[test]
fn workspace_report_tracks_each_key_separately() {
    let mut failing: HashSet<&str> = HashSet::new();

    assert_eq!(
        workspace_report(&mut failing, &"ceo", true),
        WorkspaceReport::Failed
    );
    // A different agent failing is its own first failure, not a repeat.
    assert_eq!(
        workspace_report(&mut failing, &"engineer", true),
        WorkspaceReport::Failed
    );
    assert_eq!(
        workspace_report(&mut failing, &"ceo", true),
        WorkspaceReport::StillFailing
    );
    // One recovers; the other stays failing and stays silent.
    assert_eq!(
        workspace_report(&mut failing, &"engineer", false),
        WorkspaceReport::Recovered
    );
    assert_eq!(
        workspace_report(&mut failing, &"ceo", true),
        WorkspaceReport::StillFailing
    );
    assert_eq!(
        workspace_report(&mut failing, &"ceo", false),
        WorkspaceReport::Recovered
    );
    assert!(failing.is_empty(), "a recovered key leaves no residue");
}

/// The real dispatch path against a workspace root that cannot hold a
/// directory, driven through [`HarnessPool::run`] rather than the helper.
///
/// The root is pointed at a **file**, which makes `create_dir_all` fail
/// deterministically on every platform (`ENOTDIR` / its Windows equivalent)
/// without needing permission bits a CI root user would ignore.
///
/// Asserts the reporting state, not the log text: this test binary already
/// installs a global `tracing` subscriber elsewhere
/// (`runtime::workflow_scheduler`) and asserts it wins that race, so a
/// second global capture here would make whichever test lost panic. The
/// state is what decides whether a line is emitted, so pinning it pins the
/// line count — three dispatches, one report.
#[tokio::test]
async fn a_broken_workspace_root_reports_once_across_repeated_dispatches() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A regular file where the workspace tree is expected.
    let not_a_dir = dir.path().join("workspace-root");
    std::fs::write(&not_a_dir, b"this is a file, not a directory").unwrap();

    let mut fx = fixture();
    fx.deps.workspace_root = not_a_dir.clone();
    let pool = HarnessPool::new();
    // A company id of this test's own, not the shared `acme`.
    //
    // The OpenHuman transcript root is process-wide (one
    // `OPENHUMAN_WORKSPACE` per test binary), while a session's durable
    // identity is derived from the company and agent ids. Every test that
    // runs `ceo` on the bare `acme` therefore reads and writes *one*
    // transcript, including the `{"kind":"tools"}` record. This agent is
    // built with no skills, so when it resumed a transcript another test had
    // stamped with `list_skills`/`describe_skill`/`read_skill_resource`, the
    // driver refused the turn: "session tool snapshot declares
    // non-executable tools". It only bites when the other test wins the race,
    // which is why it passed locally and failed under CI's parallelism.
    let mut rec = record();
    rec.id = CompanyId::new(format!(
        "acme-broken-workspace-{}",
        uuid::Uuid::new_v4().simple()
    ));
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    // Sanity: the condition really is a hard, repeatable failure.
    assert!(
        build::ensure_agent_workspace(&not_a_dir, &rec.id, "ceo").is_err(),
        "the test root must actually be unusable, or this proves nothing"
    );

    for turn in 0..3 {
        pool.run(
            &rec.id,
            "ceo",
            "hi",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("turn {turn} still runs without a workspace: {e:?}"));
    }

    // The turns ran — a missing workspace is not fatal, which #449 does not
    // change — and the failure is recorded exactly once.
    let failing = pool.workspace_failures.lock().unwrap();
    assert_eq!(
        failing.len(),
        1,
        "one failing agent, tracked once, however many turns it takes"
    );
    assert!(failing.contains(&(rec.id.clone(), "ceo".to_string())));
    drop(failing);

    // The next dispatch after the first is silent: only turn 1 spoke.
    assert_eq!(
        pool.note_workspace_attempt(&rec.id, "ceo", true),
        WorkspaceReport::StillFailing,
        "dispatches after the first must not re-emit the error"
    );
    // And when the volume comes back, one line says so.
    assert_eq!(
        pool.note_workspace_attempt(&rec.id, "ceo", false),
        WorkspaceReport::Recovered
    );
}

/// Issue #2306 / Codex round 2, comment 4012457318: `meter_turn_costs`
/// used to read `deps.provider` (the company default) unconditionally, so
/// every pinned agent's turn was booked to the default's telemetry
/// instead of the pinned `TenantProvider` sibling that actually served
/// it. Fixed by having every caller pass the [`HarnessModel`] the agent's
/// `Agent` was actually built against — see
/// [`CompanyAgent::chat_model`](CompanyAgent) — so this asserts the meter
/// reads THAT model's telemetry rather than `fixture()`'s "mock" default.
#[tokio::test]
async fn meter_turn_costs_books_a_pinned_agents_turn_to_its_own_provider() {
    let fx = fixture();
    // Sanity: the company default and the pinned double really do disagree,
    // or this test would pass no matter which one `meter_turn_costs` read.
    assert_eq!(fx.deps.provider.telemetry_provider_id(), "mock");

    let pinned: Arc<dyn HarnessModel> = Arc::new(PinnedProvider);
    let turn_costs = vec![TurnUsage {
        input_tokens: 100,
        output_tokens: 40,
        cached_input_tokens: 0,
        cost_usd: 0.02,
    }];

    meter_turn_costs(
        &turn_costs,
        "engineer",
        &CompanyId::new("acme"),
        &fx.deps,
        pinned.as_ref(),
        None,
    )
    .await
    .expect("meters");

    let samples = fx.meter.samples.lock().unwrap().clone();
    assert_eq!(samples.len(), 1, "one attempt, one sample: {samples:?}");
    assert_eq!(
        samples[0].provider, "pinned-provider",
        "booked to the pinned provider that actually served the turn, not \
         the company default: {samples:?}"
    );
    assert_eq!(samples[0].model, Some(crate::metering::ModelSlug::OTHER));

    let ledger = fx.store.ledger.lock().unwrap().clone();
    assert_eq!(ledger.len(), 1, "one attempt, one ledger entry: {ledger:?}");
}

/// Issue #2306 / Codex round 2, comment 4012457329 (X12): "a company
/// configured solely through agent pins builds its auxiliary passes on
/// the pin" — `pass_model` falls back to the agent's own pin once the
/// company default's own call fails, instead of an auxiliary pass (here
/// payload extraction) failing outright for a company with no default.
#[tokio::test]
async fn pass_model_falls_back_to_the_agent_pin_when_the_default_cannot_serve() {
    let mut fx = fixture();
    fx.deps.provider = Arc::new(AlwaysFailsProvider);
    let pinned: Arc<dyn HarnessModel> = Arc::new(PinnedProvider);

    let model = pass_model(&fx.deps, Some(pinned));
    let response = model
        .invoke(&(), ModelRequest::default())
        .await
        .expect("falls back to the agent's own pin rather than losing the pass");
    assert_eq!(response.text(), "ok");
    assert_eq!(
        model.telemetry_provider_id(),
        "pinned-provider",
        "telemetry follows whichever provider actually answered the call"
    );
}

/// The company default is tried FIRST on every call — `pass_model` never
/// reaches for the pin while the default can still answer it, so a
/// company running a working default keeps its auxiliary passes on the
/// shared credential rather than an agent's narrower one.
#[tokio::test]
async fn pass_model_prefers_the_company_default_while_it_resolves() {
    let fx = fixture();
    let pinned: Arc<dyn HarnessModel> = Arc::new(PinnedProvider);

    let model = pass_model(&fx.deps, Some(pinned));
    model
        .invoke(&(), ModelRequest::default())
        .await
        .expect("the company default answers");
    assert_eq!(
        model.telemetry_provider_id(),
        "mock",
        "the default is preferred while it can still serve the call"
    );
}

/// No pin at all: `pass_model` is exactly `deps.provider`, unchanged —
/// the pre-X12 behaviour every company-wide pass (title, triage,
/// planning, selector) still gets, since none of them has a single agent
/// to pin against.
#[test]
fn pass_model_with_no_pin_is_the_company_default() {
    let fx = fixture();
    let model = pass_model(&fx.deps, None);
    assert_eq!(
        model.telemetry_provider_id(),
        fx.deps.provider.telemetry_provider_id()
    );
}

/// Pins the documented inert-metering contract: until the provider reports
/// usage, a turn writes neither a ledger entry nor a usage sample.
#[tokio::test]
async fn zero_usage_turn_writes_nothing() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");
    pool.run(
        &rec.id,
        "ceo",
        "hi",
        &fx.deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("turn");

    assert!(fx.store.ledger.lock().unwrap().is_empty());
    assert!(fx.meter.samples.lock().unwrap().is_empty());
}

/// B-120, the half that made a founder's console disagree with their bill:
/// a turn that ends in an error is still written to the ledger and the usage
/// meter.
///
/// Both writes used to sit below a `?` on the turn — so a wall-clock ceiling
/// or a provider fault produced no `inference.spend` entry and no
/// `UsageSample` at all. The spend was not merely mis-displayed on the run:
/// it was never recorded anywhere, which is why the company-wide Observatory
/// total agreed that ten minutes of model work had been free.
#[tokio::test]
async fn a_failed_turn_is_still_written_to_the_ledger_and_the_meter() {
    let mut fx = fixture();
    fx.deps.provider = Arc::new(
        ScriptedProvider::new(vec![Ok(String::new())])
            .reporting_usage(tinyinference::Usage {
                input_tokens: 1_200,
                output_tokens: 340,
                total_tokens: 1_540,
                ..Default::default()
            })
            .failing_when_exhausted(),
    );
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    let outcome = pool
        .run(
            &rec.id,
            "ceo",
            "do ten minutes of work",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await;

    assert!(outcome.is_err(), "the scripted provider stays down");
    let samples = fx.meter.samples.lock().unwrap().clone();
    assert_eq!(
        samples.len(),
        1,
        "the failed turn's tokens must reach the usage meter: {samples:?}"
    );
    assert_eq!(samples[0].input_tokens, 1_200);
    assert_eq!(samples[0].output_tokens, 340);
    assert_eq!(
        fx.store.ledger.lock().unwrap().len(),
        1,
        "and its spend must reach the ledger, or the console and the bill disagree"
    );
}

/// Empty first, real reply on retry → the wrapper returns the recovered reply
/// and reports two attempts' usage (so both burnt attempts can be metered).
#[tokio::test]
#[ignore = "TODO(hive-desks follow-up): scripts the exact model-call sequence of the previous in-crate agent loop (its empty-reply retry, its iteration-cap wrap-up call, its provider-outage failure). Since plan hive-desks Phase 2 the loop is OpenHuman's own, with its own empty/cap/outage protocol; re-base the expectations on that loop once its protocol is pinned."]
async fn turn_wrapper_retries_empty_then_recovers() {
    let (agent, _deps) = scripted_agent(vec![Ok(String::new()), Ok("recovered".into())]);
    let (outcome, usages) = agent.run("hi").await;
    let outcome = outcome.expect("wrapper recovers");
    assert!(
        outcome.reply.contains("recovered"),
        "got {:?}",
        outcome.reply
    );
    assert_eq!(usages.len(), 2, "both attempts' usage is returned");
}

/// B-120: a turn that ends in a **hard error** still reports what it spent.
///
/// The failure this pins is the one a founder saw as `0 tok / $0.000` on a
/// ten-minute run: openhuman sets `last_turn_usage_totals` only after its
/// own `?`, so `read_turn_usage` reads back nothing for an attempt that
/// errored, and the usage then rode home on an `Ok` the caller never got.
///
/// The script burns a real, usage-reporting model call and then fails:
/// attempt 1 answers with usage but no text (openhuman raises
/// `EmptyProviderResponse`, so it publishes no totals), and the one-shot
/// retry hits a hard provider error. Both attempts therefore report zero of
/// their own, and the only surviving figure is the live `TurnCostUpdated`
/// tally — which is exactly what has to reach the caller *beside* the
/// `Err`, because that is what the attempt row, the ledger and the usage
/// meter are all built from.
#[tokio::test]
async fn a_hard_failed_turn_still_reports_the_tokens_it_burned() {
    let (agent, _deps) = scripted_agent_over(
        ScriptedProvider::new(vec![Ok(String::new())])
            .reporting_usage(tinyinference::Usage {
                input_tokens: 1_200,
                output_tokens: 340,
                total_tokens: 1_540,
                ..Default::default()
            })
            .failing_when_exhausted(),
    );

    let (outcome, usages) = agent.run("do ten minutes of work").await;

    assert!(
        outcome.is_err(),
        "the provider is permanently down past the first call; the turn must fail: {:?}",
        outcome.as_ref().map(|o| o.reply.clone())
    );
    let tokens: u64 = usages
        .iter()
        .map(|u| u.input_tokens + u.output_tokens)
        .sum();
    assert_eq!(
        tokens, 1_540,
        "a failed turn must carry home the tokens its own model call burned, \
         not report itself as free: {usages:?}"
    );
}

/// CodeRabbit review (PR #2053): a turn that fails WITHOUT spending
/// anything must not inherit an earlier, unrelated turn's totals off the
/// **reused** `Agent` — the same "0 tok / $0.000" bug B-120 fixes, in the
/// opposite direction: a turn that spent nothing must not be billed for
/// what a PAST turn on this same agent already spent and was already
/// billed for.
///
/// `CompanyAgent` reuses one `Agent` for every chat of a `(company,
/// agent_id)` pair, and openhuman finalizes `last_turn_usage_totals` only
/// on a turn that completes normally — an attempt that ends in
/// `EmptyProviderResponse` never touches it, so a naive read after such an
/// attempt reads back whatever the LAST *successful* turn on this agent
/// left there, not this attempt's own (zero) spend. Left unguarded, that
/// stale figure — already billed once when the first turn settled — would
/// be billed a second time on a completely different, later turn that
/// made no metered call at all.
///
/// Turn 1 succeeds in one attempt and spends 1,540 tokens for real — the
/// exact figure `last_turn_usage_totals` is left holding. Turn 2, on the
/// SAME agent, scripts an immediate blank (`EmptyProviderResponse`) and
/// then a hard failure on the one-shot retry once the script is exhausted
/// — the identical shape `a_hard_failed_turn_still_reports_the_tokens_it_burned`
/// already pins, just as the SECOND top-level call on this agent rather
/// than the first. Because this provider carries usage on every scripted
/// reply, turn 2's own first attempt genuinely burns another 1,540 tokens
/// before dying, which the live `TurnCostUpdated` tally still recovers
/// (`last_observed_turn_cost`) — so the correct total is 1,540 exactly,
/// not 3,080 (turn 1's stale total, read back and double-counted across
/// turn 2's own two attempts on top of what turn 2 itself burned).
#[tokio::test]
async fn a_turn_that_burns_nothing_does_not_inherit_a_past_turns_stale_total() {
    let (agent, _deps) = scripted_agent_over(
        ScriptedProvider::new(vec![
            Ok("turn one finished cleanly".to_string()),
            Ok(String::new()),
        ])
        .reporting_usage(tinyinference::Usage {
            input_tokens: 1_200,
            output_tokens: 340,
            total_tokens: 1_540,
            ..Default::default()
        })
        .failing_when_exhausted(),
    );

    // Turn one: a real, one-attempt success. Leaves
    // `last_turn_usage_totals` holding 1,540 tokens.
    let (outcome, usages) = agent.run("turn one").await;
    outcome.expect("turn one is a clean, successful reply");
    assert_eq!(
        usages
            .iter()
            .map(|u| u.input_tokens + u.output_tokens)
            .sum::<u64>(),
        1_540,
        "turn one's own real spend"
    );

    // Turn two, same agent: attempt 1 consumes the scripted blank
    // (EmptyProviderResponse), the retry then finds the script exhausted
    // and hits the permanent failure — both attempts error, neither
    // finalizes `last_turn_usage_totals`, and without the fix both reads
    // would instead return turn one's already-billed 1,540 a second AND
    // third time.
    let (second_outcome, second_usages) = agent.run("turn two").await;
    assert!(
        second_outcome.is_err(),
        "turn two's provider is permanently down past its first call: {:?}",
        second_outcome.as_ref().map(|o| o.reply.clone())
    );
    let second_tokens: u64 = second_usages
        .iter()
        .map(|u| u.input_tokens + u.output_tokens)
        .sum();
    assert_eq!(
        second_tokens, 1_540,
        "turn two must report its OWN spend — one metered call, recovered via the live \
         progress-stream tally since its own attempt also errors before finalizing totals \
         — never turn one's already-billed total read back a second and third time: \
         {second_usages:?}"
    );
}

/// Codex review (PR #2053): the original recovery gate only fired when
/// EVERY attempt in `usages` reported zero, so it could recover at most
/// one attempt's spend. A metered first attempt that empties, followed by
/// a retry that succeeds and publishes its OWN authoritative total, left
/// `usages` as `[zero, retry_total]` — not all-zero — so the first
/// attempt's already-published `TurnCostUpdated` spend was silently
/// dropped rather than merely under-reported.
///
/// Both scripted replies carry the SAME usage (1,000 tokens each, via the
/// one shared `.reporting_usage(...)` every `ScriptedProvider` reply
/// gets), so the only way the total comes out to 2,000 rather than 1,000
/// is if the first attempt's spend — recovered from its OWN segment of
/// the progress stream, per `attempt_event_segments` — survives instead
/// of being discarded the moment the retry's real total makes `usages`
/// not-all-zero.
#[tokio::test]
#[ignore = "TODO(hive-desks follow-up): scripts the exact model-call sequence of the previous in-crate agent loop (its empty-reply retry, its iteration-cap wrap-up call, its provider-outage failure). Since plan hive-desks Phase 2 the loop is OpenHuman's own, with its own empty/cap/outage protocol; re-base the expectations on that loop once its protocol is pinned."]
async fn a_metered_empty_attempt_is_still_recovered_when_the_retry_succeeds() {
    let (agent, _deps) = scripted_agent_over(
        ScriptedProvider::new(vec![Ok(String::new()), Ok("recovered".to_string())])
            .reporting_usage(tinyinference::Usage {
                input_tokens: 800,
                output_tokens: 200,
                total_tokens: 1_000,
                ..Default::default()
            }),
    );

    let (outcome, usages) = agent.run("hi").await;
    let outcome = outcome.expect("the retry recovers a real reply");
    assert!(
        outcome.reply.contains("recovered"),
        "got {:?}",
        outcome.reply
    );
    assert_eq!(usages.len(), 2, "both attempts' usage is returned");

    let tokens: u64 = usages
        .iter()
        .map(|u| u.input_tokens + u.output_tokens)
        .sum();
    assert_eq!(
        tokens, 2_000,
        "both attempts genuinely burned 1,000 tokens each — the first attempt's spend must \
         not be dropped just because the retry went on to publish its own (also real) \
         total: {usages:?}"
    );
}
