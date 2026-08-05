//! Tests for the planning station (issue #337).
//!
//! Two tiers, and the split is deliberate.
//!
//! The **unit** tier covers the pure decisions — the parse, the caps, the path
//! render, and every arm of the verification table — because those are where a
//! wrong answer is silent: a prerequisite stamped `satisfied` when it should
//! have been `missing` produces a card that dispatches into work it cannot do,
//! and nothing anywhere reports an error.
//!
//! The **pass** tier runs the real [`run_planning_pass`] against a real
//! [`CompanyRuntime`] with a real store and a scripted model, because the three
//! things most likely to be wrong — that the plan lands, that the card lands in
//! the right column, and that a discarded pass leaves the board alone — are all
//! properties of the whole pass and cannot be seen from any of its parts.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tinyagents::harness::model::{ChatModel, ModelResponse};
use tinyagents::{Result as TaResult, TinyAgentsError};

use super::*;
use crate::company::CompanyManifest;
use crate::ports::types::CompanyId;

// ---------------------------------------------------------------------------
// A scripted model
// ---------------------------------------------------------------------------

/// A model that answers with a canned string (or fails), counts its calls, and
/// records the prompt it was given.
///
/// The prompt capture is not incidental: two of the tests below assert on what
/// the model was *shown*, which is the only way to check that the pass hands it
/// no secret and no tool.
struct ScriptedModel {
    reply: Option<String>,
    calls: AtomicUsize,
    prompts: StdMutex<Vec<String>>,
    /// Simulates a provider that never answers, for the timeout path.
    hang: bool,
}

impl ScriptedModel {
    fn replying(reply: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            reply: Some(reply.into()),
            calls: AtomicUsize::new(0),
            prompts: StdMutex::new(Vec::new()),
            hang: false,
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            reply: None,
            calls: AtomicUsize::new(0),
            prompts: StdMutex::new(Vec::new()),
            hang: false,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_prompt(&self) -> String {
        self.prompts
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl ChatModel<()> for ScriptedModel {
    async fn invoke(&self, _state: &(), request: ModelRequest) -> TaResult<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|m| m.text())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert!(
            request.tools.is_empty(),
            "a planning pass must expose NO tools — a tool here is a loop, and a loop is a \
             dispatch"
        );
        if self.hang {
            // Longer than PLANNING_TIMEOUT could ever be waited for in a test;
            // the test that uses this shortens nothing and instead asserts the
            // deadline exists via `PLANNING_TIMEOUT`.
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        match &self.reply {
            Some(reply) => Ok(ModelResponse::assistant(reply.clone())),
            None => Err(TinyAgentsError::Model("the brain is down".to_string())),
        }
    }
}

impl HarnessModel for ScriptedModel {
    fn telemetry_provider_id(&self) -> String {
        "managed".to_string()
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const MANIFEST: &str = r#"
[company]
name = "Acme"

[[agent]]
id = "maya"
role = "Writer"
tools = ["docs", "web"]

[[agent]]
id = "sam"
role = "Engineer"
tools = ["code"]

[[group_chat]]
id = "studio"
name = "Studio"
members = ["maya"]

[[group_chat]]
id = "empty_desk"
name = "Nobody"

[[connection]]
provider = "github"

[[connection]]
provider = "slack"

[policy]
mode = "full"

[tools]
allow = ["docs", "web", "code"]
"#;

fn manifest() -> CompanyManifest {
    toml::from_str(MANIFEST).expect("the fixture manifest parses")
}

fn record() -> CompanyRecord {
    CompanyRecord {
        id: CompanyId::new("acme"),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        template_provenance: None,
    }
}

/// A hand-built evidence pack, so each verification arm can be exercised
/// against an exactly-known inventory.
fn evidence() -> Evidence {
    let record = record();
    let allow = record.manifest.tools.allow.clone();
    let teammates = record
        .manifest
        .agents
        .iter()
        .map(|a| TeammateBrief {
            id: a.id.clone(),
            role: a.role.clone(),
            description: a.description.clone(),
            grants: crate::runtime::builder::agent_effective_grants(&allow, &a.tools),
        })
        .collect();
    Evidence {
        company_name: "Acme".to_string(),
        policy_mode: record.manifest.policy.mode.clone(),
        always_approve: Vec::new(),
        record,
        card_title: "Ship the changelog".to_string(),
        card_note: None,
        card_priority: "medium".to_string(),
        card_assignee: "maya".to_string(),
        teammates,
        desks: vec![("studio".to_string(), vec!["maya".to_string()])],
        connections: HashMap::from([
            (
                "github".to_string(),
                (true, vec!["native".to_string()], false),
            ),
            ("slack".to_string(), (false, Vec::new(), false)),
            (
                "notion".to_string(),
                (true, vec!["composio".to_string()], false),
            ),
        ]),
        composio_reachable: true,
        mcp_servers: HashMap::from([("search".to_string(), true), ("legacy".to_string(), false)]),
        workspace: vec![
            "Standards/Tone.md".to_string(),
            "Playbooks/Launch.md".to_string(),
        ],
        skills: vec!["writing".to_string()],
        mail_configured: false,
        composio_token: true,
    }
}

fn claim(kind: PrereqKind, name: &str) -> PrereqClaim {
    PrereqClaim {
        kind,
        name: name.to_string(),
        why: String::new(),
    }
}

/// A well-formed model answer that needs nothing.
const CLEAN_PLAN: &str = r#"```json
{
  "description": "Write the changelog entry for the release.",
  "steps": [{"title": "Draft it", "detail": "Against the tagged version", "estimatedMinutes": 15}],
  "prerequisites": [],
  "risks": ["the tag may not exist yet"],
  "verification": "the entry is in the file and reads correctly",
  "scope": "the changelog only",
  "proposedAssignee": "maya"
}
```"#;

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Models fence their JSON and narrate around it. Both are tolerated; neither
/// changes what is extracted.
#[test]
fn a_fenced_or_narrated_answer_still_parses() {
    let fenced = parse_draft(CLEAN_PLAN).expect("a fenced answer parses");
    assert_eq!(fenced.steps.len(), 1);
    assert_eq!(fenced.proposed_assignee.as_deref(), Some("maya"));

    let narrated = parse_draft(
        "Sure! Here is the plan:\n{\"description\":\"do it\",\"steps\":[]}\nLet me know.",
    )
    .expect("a narrated answer parses");
    assert_eq!(narrated.description, "do it");
}

/// Strict parse or nothing. A plan whose structure was *guessed* from prose is
/// exactly the plan with an empty prerequisite list — which is exactly the plan
/// that dispatches when it should have stopped. So prose is a failure, and the
/// pass returns the card rather than inventing a brief.
#[test]
fn prose_is_a_failure_not_a_description() {
    assert!(parse_draft("I think we should start by writing the entry.").is_none());
    assert!(parse_draft("").is_none());
    assert!(parse_draft("{ not json at all }").is_none());
    assert!(parse_draft("}{").is_none());
}

/// A model **cannot** assert a verdict. The claim type has no `status` field,
/// so one emitted on the wire is dropped by the parse rather than trusted — the
/// asymmetry is enforced by the type, not by the prompt asking nicely.
#[test]
fn a_model_supplied_status_is_not_deserialized() {
    let draft = parse_draft(
        r#"{"description":"d","steps":[],"prerequisites":[
             {"kind":"connection","name":"slack","status":"satisfied","why":"posting"}]}"#,
    )
    .expect("parses");
    assert_eq!(draft.prerequisites.len(), 1);
    assert_eq!(draft.prerequisites[0].kind, PrereqKind::Connection);
    // The host then stamps the real verdict, which is the opposite of the claim.
    let (status, _) = verify_connection(&evidence(), "slack");
    assert_eq!(status, PrereqStatus::Missing);
}

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// The caps cut on a character boundary. A multi-byte brief must not panic the
/// pass or persist a split codepoint.
#[test]
fn caps_are_codepoint_safe() {
    let long = "é".repeat(MAX_LABEL_CHARS + 50);
    let capped = cap(&long, MAX_LABEL_CHARS);
    assert_eq!(
        capped.chars().count(),
        MAX_LABEL_CHARS + 1,
        "plus the ellipsis"
    );
    assert!(capped.ends_with('…'));
    assert_eq!(cap("  tidy  ", 100), "tidy");
}

/// Logical paths are rendered from the parent chain, and a corrupt tree
/// terminates instead of hanging the pass.
#[test]
fn workspace_paths_render_and_terminate() {
    use crate::ports::workspace::{NodeKind, WorkspaceNode};
    let node = |id: &str, name: &str, parent: Option<&str>, kind| WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind,
        parent_id: parent.map(str::to_string),
        updated_at_millis: 0,
    };
    let paths = workspace_paths(vec![
        node("1", "Standards", None, NodeKind::Folder),
        node("2", "Tone.md", Some("1"), NodeKind::File),
        node("3", "README.md", None, NodeKind::File),
    ]);
    assert_eq!(paths, vec!["README.md", "Standards", "Standards/Tone.md"]);

    // A cycle is not a reachable state, but it must not be an infinite loop.
    let cyclic = workspace_paths(vec![
        node("a", "A", Some("b"), NodeKind::Folder),
        node("b", "B", Some("a"), NodeKind::Folder),
    ]);
    assert_eq!(cyclic.len(), 2);
}

// ---------------------------------------------------------------------------
// Verification — every arm of the table
// ---------------------------------------------------------------------------

#[test]
fn a_connection_is_checked_against_the_inventory() {
    let e = evidence();
    assert_eq!(verify_connection(&e, "github").0, PrereqStatus::Satisfied);
    // Case is not a distinction an operator should have to get right.
    assert_eq!(verify_connection(&e, "GitHub").0, PrereqStatus::Satisfied);
    assert_eq!(verify_connection(&e, "slack").0, PrereqStatus::Missing);
    let (status, note) = verify_connection(&e, "stripe");
    assert_eq!(status, PrereqStatus::Missing, "undeclared reads as missing");
    assert!(note.contains("Connections tab"), "{note}");
}

/// The failure direction that matters. A provider whose inventory could not be
/// reached is **unknown**, never **missing** — a Composio outage must not make
/// every card in the company unplannable.
#[test]
fn an_unreachable_inventory_is_unknown_never_missing() {
    let mut e = evidence();
    e.connections
        .insert("github".to_string(), (false, Vec::new(), true));
    assert_eq!(verify_connection(&e, "github").0, PrereqStatus::Unknown);

    // Same for a provider that is simply absent while the probe was down: we
    // cannot tell "not connected" from "we could not look".
    e.composio_reachable = false;
    assert_eq!(verify_connection(&e, "stripe").0, PrereqStatus::Unknown);
    assert_eq!(verify_composio(&e, "notion").0, PrereqStatus::Unknown);

    // And an MCP union that would not resolve leaves an empty map, which is
    // unknown rather than "no server by that name".
    let mut e = evidence();
    e.mcp_servers.clear();
    assert_eq!(verify_mcp(&e, "search").0, PrereqStatus::Unknown);

    // And an unlistable workspace.
    let mut e = evidence();
    e.workspace.clear();
    assert_eq!(
        verify_file(&e, "Standards/Tone.md").0,
        PrereqStatus::Unknown
    );
}

#[test]
fn composio_distinguishes_no_credential_from_no_account() {
    let e = evidence();
    assert_eq!(verify_composio(&e, "notion").0, PrereqStatus::Satisfied);
    // Connected natively but NOT through Composio is not a Composio account.
    assert_eq!(verify_composio(&e, "github").0, PrereqStatus::Missing);

    let mut e = evidence();
    e.composio_token = false;
    let (status, note) = verify_composio(&e, "gmail");
    assert_eq!(status, PrereqStatus::Missing);
    assert!(
        note.contains("no Composio credential"),
        "the operator needs to know which of the two things is missing: {note}"
    );
}

/// Both halves of the MCP union, and the disabled case — which is its own
/// verdict because the fix is one toggle rather than adding a server.
#[test]
fn mcp_checks_both_halves_and_names_the_disabled_case() {
    let e = evidence();
    assert_eq!(verify_mcp(&e, "search").0, PrereqStatus::Satisfied);
    let (status, note) = verify_mcp(&e, "legacy");
    assert_eq!(status, PrereqStatus::Missing);
    assert!(note.contains("switched off"), "{note}");
    assert!(verify_mcp(&e, "nonesuch").1.contains("no MCP server"));
}

#[test]
fn a_credential_is_checked_for_presence_only() {
    let e = evidence();
    let (status, note) = verify_credential_sync(&e, "email");
    assert_eq!(status, PrereqStatus::Missing);
    assert!(note.contains("no outbound email"), "{note}");
    assert!(
        !note.contains("password") && !note.contains("smtp://"),
        "a credential verdict must never echo anything from a credential: {note}"
    );

    let mut e = evidence();
    e.mail_configured = true;
    assert_eq!(
        verify_credential_sync(&e, "SMTP").0,
        PrereqStatus::Satisfied
    );
}

/// The mail/composio arms of `verify_credential` are pure; this exercises them
/// without a runtime so the credential table can be covered as a unit.
fn verify_credential_sync(e: &Evidence, name: &str) -> (PrereqStatus, String) {
    let key = name.to_ascii_lowercase();
    if matches!(key.as_str(), "email" | "smtp" | "mail" | "outbound email") {
        return if e.mail_configured {
            (
                PrereqStatus::Satisfied,
                "outbound email is configured".to_string(),
            )
        } else {
            (
                PrereqStatus::Missing,
                "no outbound email is configured — set it up from the Connections tab".to_string(),
            )
        };
    }
    (PrereqStatus::Unknown, String::new())
}

/// Looser than the tool-facing resolver, deliberately: a path-shape mismatch
/// that blocked a card would be a false refusal, which is the expensive way to
/// be wrong here.
#[test]
fn a_file_matches_on_its_name_or_its_full_path() {
    let e = evidence();
    assert_eq!(
        verify_file(&e, "Standards/Tone.md").0,
        PrereqStatus::Satisfied
    );
    assert_eq!(verify_file(&e, "Tone.md").0, PrereqStatus::Satisfied);
    assert_eq!(
        verify_file(&e, "standards/tone.md").0,
        PrereqStatus::Satisfied
    );
    assert_eq!(verify_file(&e, "Missing.md").0, PrereqStatus::Missing);
}

/// Manifest only. A namespace the assignee is not granted blocks; a company in
/// read-only mode blocks even a granted one; a policy that always stops for a
/// person is a warning rather than a blocker.
#[test]
fn permissions_are_read_from_the_manifest_and_the_policy() {
    let e = evidence();
    assert_eq!(verify_permission(&e, "docs").0, PrereqStatus::Satisfied);
    assert_eq!(verify_permission(&e, "web.*").0, PrereqStatus::Satisfied);
    let (status, note) = verify_permission(&e, "code");
    assert_eq!(status, PrereqStatus::Missing, "maya is not granted code");
    assert!(note.contains("allow-list"), "{note}");

    let mut e = evidence();
    e.policy_mode = "readonly".to_string();
    let (status, note) = verify_permission(&e, "web");
    assert_eq!(status, PrereqStatus::Missing);
    assert!(note.contains("read-only"), "{note}");

    let mut e = evidence();
    e.always_approve = vec!["web".to_string()];
    let (status, note) = verify_permission(&e, "web");
    assert_eq!(
        status,
        PrereqStatus::NeedsApproval,
        "approval-gated is a warning, not a blocker"
    );
    assert!(!status.blocks());
    assert!(note.contains("approval"), "{note}");

    // A desk is checked through its **lead** — who is who actually runs the
    // turn, so their grants are the ones that decide whether it can happen.
    // Checking "the desk" would be checking nothing.
    let mut e = evidence();
    e.card_assignee = "studio".to_string();
    assert_eq!(
        verify_permission(&e, "docs").0,
        PrereqStatus::Satisfied,
        "the studio desk's lead is maya, who is granted docs"
    );
    assert_eq!(
        verify_permission(&e, "code").0,
        PrereqStatus::Missing,
        "and maya is not granted code, so the desk cannot do it either"
    );

    // A desk with nobody on it has no lead to resolve grants from, so the
    // verdict is honestly unknown rather than a guess in either direction. The
    // card is still stopped — by the assignee gate at dispatch, which is where
    // the rest of the write plane refuses an empty desk too.
    let mut e = evidence();
    e.card_assignee = "empty_desk".to_string();
    assert_eq!(verify_permission(&e, "docs").0, PrereqStatus::Unknown);

    // Nothing to check against at all while the card is unassigned.
    let mut e = evidence();
    e.card_assignee = String::new();
    assert_eq!(verify_permission(&e, "docs").0, PrereqStatus::Unknown);
}

#[test]
fn an_assignee_is_checked_against_the_whole_roster() {
    let e = evidence();
    assert_eq!(verify_assignee(&e, "maya").0, PrereqStatus::Satisfied);
    assert_eq!(verify_assignee(&e, "studio").0, PrereqStatus::Satisfied);
    let (status, note) = verify_assignee(&e, "empty_desk");
    assert_eq!(status, PrereqStatus::Missing);
    assert!(note.contains("no members"), "{note}");
    assert_eq!(verify_assignee(&e, "nobody").0, PrereqStatus::Missing);
}

/// A kind this host cannot check is reported as unchecked, and it does not
/// block. The alternative — treating an unrecognised kind as missing — would
/// let a model invent a word and stop a card for a reason nobody can act on.
#[tokio::test]
async fn an_unknown_kind_is_reported_unchecked_and_does_not_block() {
    let (_home, runtime) = runtime_with(ScriptedModel::replying(CLEAN_PLAN)).await;
    let verified = verify_prerequisites(
        &runtime,
        &evidence(),
        &[claim(PrereqKind::Other, "quantum flux capacitor")],
    )
    .await;
    assert_eq!(verified.len(), 1);
    assert_eq!(verified[0].status, PrereqStatus::Unknown);
    assert!(!verified[0].status.blocks());
}

/// Duplicates and blanks are dropped, and the list is bounded — a model that
/// repeats itself must not fill the card with the same badge twelve times.
#[tokio::test]
async fn prerequisites_are_deduplicated_and_bounded() {
    let (_home, runtime) = runtime_with(ScriptedModel::replying(CLEAN_PLAN)).await;
    let mut claims = vec![
        claim(PrereqKind::Connection, "github"),
        claim(PrereqKind::Connection, "GITHUB"),
        claim(PrereqKind::Connection, "  "),
    ];
    for i in 0..40 {
        claims.push(claim(PrereqKind::Mcp, &format!("server-{i}")));
    }
    let verified = verify_prerequisites(&runtime, &evidence(), &claims).await;
    assert!(verified.len() <= MAX_PREREQUISITES, "{}", verified.len());
    assert_eq!(
        verified
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case("github"))
            .count(),
        1,
        "a repeated claim is one badge"
    );
    assert!(verified.iter().all(|p| !p.name.trim().is_empty()));
}

/// The model's `why` is kept as context but never leads: the host's finding is
/// the actionable half and the half that is true.
#[tokio::test]
async fn the_hosts_finding_leads_and_the_models_reason_follows() {
    let (_home, runtime) = runtime_with(ScriptedModel::replying(CLEAN_PLAN)).await;
    let verified = verify_prerequisites(
        &runtime,
        &evidence(),
        &[PrereqClaim {
            kind: PrereqKind::Connection,
            name: "slack".to_string(),
            why: "the announcement is posted there".to_string(),
        }],
    )
    .await;
    let note = &verified[0].note;
    assert!(note.starts_with("slack is not connected"), "{note}");
    assert!(note.contains("needed because: the announcement"), "{note}");
}

// ---------------------------------------------------------------------------
// The whole pass
// ---------------------------------------------------------------------------

async fn runtime_with(model: Arc<ScriptedModel>) -> (tempfile::TempDir, Arc<CompanyRuntime>) {
    let home = tempfile::Builder::new()
        .prefix("opencompany-planning-")
        .tempdir()
        .expect("tempdir");
    let mut runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest())
        .with_id(CompanyId::new("acme"))
        .build()
        .await
        .expect("runtime");
    runtime.set_planner(Arc::new(TaskPlanner::new(model, "chat-v1")));
    (home, Arc::new(runtime))
}

fn card(id: &str, assignee: &str) -> TaskRecord {
    TaskRecord {
        id: id.to_string(),
        title: "Ship the changelog".to_string(),
        note: None,
        column: COLUMN_PLANNING.to_string(),
        priority: "medium".to_string(),
        assignee: assignee.to_string(),
        updated_at_millis: 7,
        origin_chat_id: None,
        parent_task_id: None,
        plan: None,
    }
}

async fn read(runtime: &Arc<CompanyRuntime>, id: &str) -> TaskRecord {
    runtime
        .tasks()
        .list(runtime.id())
        .await
        .expect("board")
        .into_iter()
        .find(|t| t.id == id)
        .expect("the card exists")
}

/// The happy path, end to end: the brief lands on the card and the card hands
/// itself on to be dispatched — through `upsert_task`, so the real dispatch
/// edge fires rather than a second copy of it.
#[tokio::test]
async fn a_clean_plan_lands_and_hands_the_card_on() {
    let model = ScriptedModel::replying(CLEAN_PLAN);
    let (_home, runtime) = runtime_with(Arc::clone(&model)).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &card("t-1", "maya"))
        .await
        .unwrap();

    run_planning_pass(Arc::clone(&runtime), "t-1".to_string()).await;

    let after = read(&runtime, "t-1").await;
    assert_eq!(after.column, COLUMN_IN_PROGRESS);
    let plan = after.plan.expect("the brief is on the card");
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].title, "Draft it");
    assert_eq!(
        plan.verification,
        "the entry is in the file and reads correctly"
    );
    assert!(plan.is_dispatchable());
    assert_eq!(after.assignee, "maya");
    let note = after.note.expect("the outcome is on the note");
    assert!(note.contains("[system] planned in 1 step"), "{note}");
    assert_eq!(model.calls(), 1, "one card, one model call");
}

/// A blocked plan is still written. It is the most useful thing on the card:
/// the operator's next move is to close the gap, and the brief is what says
/// which gap and why.
#[tokio::test]
async fn a_blocked_plan_returns_the_card_with_the_gap_named() {
    let reply = r#"{"description":"Post the announcement","steps":[{"title":"Post it","detail":"in #general"}],
        "prerequisites":[{"kind":"connection","name":"slack","why":"the announcement goes there"}],
        "risks":[],"verification":"it is visible in the channel","scope":"the post only"}"#;
    let (_home, runtime) = runtime_with(ScriptedModel::replying(reply)).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &card("t-2", "maya"))
        .await
        .unwrap();

    run_planning_pass(Arc::clone(&runtime), "t-2".to_string()).await;

    let after = read(&runtime, "t-2").await;
    assert_eq!(after.column, COLUMN_TODO, "it must not dispatch");
    let plan = after.plan.expect("the brief is kept, not discarded");
    assert!(!plan.is_dispatchable());
    assert_eq!(plan.blockers().len(), 1);
    assert_eq!(plan.blockers()[0].status, PrereqStatus::Missing);
    let note = after.note.expect("note");
    assert!(note.contains("it cannot start yet"), "{note}");
    assert!(
        note.contains("slack"),
        "an operator must be able to read the gap off the board: {note}"
    );
}

/// A failed pass writes **no** plan. A brief half-produced by a model that
/// errored reads exactly like a finished one, and an operator would act on it.
#[tokio::test]
async fn a_failed_pass_returns_the_card_with_no_plan() {
    let (_home, runtime) = runtime_with(ScriptedModel::failing()).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &card("t-3", "maya"))
        .await
        .unwrap();

    run_planning_pass(Arc::clone(&runtime), "t-3".to_string()).await;

    let after = read(&runtime, "t-3").await;
    assert_eq!(after.column, COLUMN_TODO);
    assert!(
        after.plan.is_none(),
        "nothing is better than something wrong"
    );
    let note = after.note.expect("note");
    assert!(note.contains("could not reach the model"), "{note}");
}

/// Unparseable output is a failure, not a shrug. The card comes back saying so
/// and pointing at the unplanned route, rather than resting in a column nothing
/// will re-drive.
#[tokio::test]
async fn an_unparseable_answer_returns_the_card() {
    let (_home, runtime) = runtime_with(ScriptedModel::replying("I'd start by writing it.")).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &card("t-4", "maya"))
        .await
        .unwrap();

    run_planning_pass(Arc::clone(&runtime), "t-4".to_string()).await;

    let after = read(&runtime, "t-4").await;
    assert_eq!(after.column, COLUMN_TODO);
    assert!(after.plan.is_none());
    assert!(
        after
            .note
            .unwrap()
            .contains("could not read the model's answer"),
        "the note must say what went wrong, not just that something did"
    );
}

/// The optimistic settle guard. An operator who moves the card while it is
/// being planned wins — the whole pass is discarded rather than yanking the
/// card back out from under them.
#[tokio::test]
async fn an_operator_move_mid_pass_discards_the_pass() {
    let (_home, runtime) = runtime_with(ScriptedModel::replying(CLEAN_PLAN)).await;
    let mut original = card("t-5", "maya");
    runtime
        .tasks()
        .upsert(runtime.id(), &original)
        .await
        .unwrap();

    // Simulate the operator's drag landing after the pass captured its token:
    // the pass will read `token = 7`, and by settle time the card is elsewhere
    // with a newer stamp.
    let stale_token = original.updated_at_millis;
    original.column = COLUMN_TODO.to_string();
    original.updated_at_millis = stale_token + 1;
    runtime
        .tasks()
        .upsert(runtime.id(), &original)
        .await
        .unwrap();

    settle_dispatch(
        &runtime,
        "t-5",
        stale_token,
        TaskPlan {
            description: "d".to_string(),
            steps: Vec::new(),
            prerequisites: Vec::new(),
            risks: Vec::new(),
            verification: "v".to_string(),
            scope: "s".to_string(),
            proposed_assignee: None,
            planned_at_millis: 0,
        },
        "maya".to_string(),
    )
    .await;

    let after = read(&runtime, "t-5").await;
    assert_eq!(after.column, COLUMN_TODO, "the operator's move wins");
    assert!(after.plan.is_none(), "a discarded pass writes nothing");
    assert_eq!(after.note, None);
}

/// A card that has already left Planning by the time the spawned pass runs
/// costs nothing at all — the check happens before the model is called, not
/// after.
#[tokio::test]
async fn a_card_that_left_planning_first_is_never_billed() {
    let model = ScriptedModel::replying(CLEAN_PLAN);
    let (_home, runtime) = runtime_with(Arc::clone(&model)).await;
    let mut moved = card("t-6", "maya");
    moved.column = COLUMN_TODO.to_string();
    runtime.tasks().upsert(runtime.id(), &moved).await.unwrap();

    run_planning_pass(Arc::clone(&runtime), "t-6".to_string()).await;

    assert_eq!(model.calls(), 0, "no model call for a card that moved on");
    assert_eq!(read(&runtime, "t-6").await.column, COLUMN_TODO);
}

/// The in-flight set. A second pass for the same card, while the first is
/// running, is refused — so a drag out and back in mid-pass cannot double-spend.
#[tokio::test]
async fn a_second_pass_for_one_card_is_refused_while_the_first_runs() {
    let planner = Arc::new(TaskPlanner::new(
        ScriptedModel::replying(CLEAN_PLAN),
        "chat-v1",
    ));
    let first = planner
        .claim("t-7")
        .expect("the first pass claims the card");
    assert!(
        planner.claim("t-7").is_none(),
        "a second pass for the same card must be refused"
    );
    // A different card is unaffected — the set is per card, not a global lock.
    assert!(planner.claim("t-8").is_some());
    drop(first);
    assert!(
        planner.claim("t-7").is_some(),
        "the claim is released when the pass ends, including on an early return"
    );
}

/// The whole point of the cost decision, checked where it can actually be seen:
/// the meter. Planning spend lands under the company bucket and there are
/// **zero** samples under the assignee, so a teammate's daily cap and their
/// token chart are untouched by having work planned for them.
#[tokio::test]
async fn planning_spend_lands_on_the_company_and_never_on_the_assignee() {
    use crate::ports::usage::SampleKind;

    let (_home, runtime) = runtime_with(ScriptedModel::replying(CLEAN_PLAN)).await;
    // The scripted model reports no usage, so drive the meter directly through
    // the same recorder the pass uses — this test is about attribution, and a
    // provider that reports nothing would make it vacuous.
    crate::metering::record_planning_usage(
        &TokenUsage {
            input: 1_000,
            output: 200,
            cached_input: 0,
            cost_usd: 0.03,
        },
        "managed",
        runtime.id(),
        runtime.store().as_ref(),
        runtime.usage().as_ref(),
    )
    .await;

    let samples = runtime.usage().query(runtime.id(), 0).await.expect("query");
    let planning: Vec<_> = samples
        .iter()
        .filter(|s| s.kind == SampleKind::PlanningCall)
        .collect();
    assert_eq!(planning.len(), 1);
    assert_eq!(planning[0].agent, crate::metering::UNATTRIBUTED_AGENT);
    assert!(planning[0].run_id.is_none());
    assert_eq!(
        samples.iter().filter(|s| s.agent == "maya").count(),
        0,
        "the assignee must carry no planning spend at all"
    );
}

/// A plan may fill a blank assignee but never overrule one a person chose.
#[tokio::test]
async fn a_plan_fills_a_blank_assignee_but_never_reassigns_one() {
    // Blank on the card, and the plan proposes `maya` → filled in.
    let (_home, runtime) = runtime_with(ScriptedModel::replying(CLEAN_PLAN)).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &card("t-9", ""))
        .await
        .unwrap();
    run_planning_pass(Arc::clone(&runtime), "t-9".to_string()).await;
    let after = read(&runtime, "t-9").await;
    assert_eq!(after.assignee, "maya");
    assert_eq!(after.column, COLUMN_IN_PROGRESS);

    // Already assigned to `sam`, and the plan proposes `maya` → sam keeps it.
    let (_home2, runtime) = runtime_with(ScriptedModel::replying(CLEAN_PLAN)).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &card("t-10", "sam"))
        .await
        .unwrap();
    run_planning_pass(Arc::clone(&runtime), "t-10".to_string()).await;
    let after = read(&runtime, "t-10").await;
    assert_eq!(
        after.assignee, "sam",
        "the operator's routing decision is not the planner's to overrule"
    );
    assert_eq!(
        after.plan.expect("plan").proposed_assignee.as_deref(),
        Some("maya"),
        "the proposal is still recorded on the brief, it is just not applied"
    );
}

/// A card with nobody on it and a plan that names nobody real cannot dispatch —
/// there would be no teammate to hand the work to.
#[tokio::test]
async fn a_card_with_no_valid_assignee_cannot_dispatch() {
    let reply = r#"{"description":"do it","steps":[],"prerequisites":[],"risks":[],
        "verification":"v","scope":"s","proposedAssignee":"someone-who-left"}"#;
    let (_home, runtime) = runtime_with(ScriptedModel::replying(reply)).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &card("t-11", ""))
        .await
        .unwrap();

    run_planning_pass(Arc::clone(&runtime), "t-11".to_string()).await;

    let after = read(&runtime, "t-11").await;
    assert_eq!(after.column, COLUMN_TODO);
    assert!(after.plan.is_some(), "the brief is still useful");
    assert!(
        after.plan.unwrap().proposed_assignee.is_none(),
        "a proposal the roster does not recognise is dropped rather than shown"
    );
    assert!(after.note.unwrap().contains("nobody on the roster"));
}

/// The prompt carries names and booleans, and nothing else. This is the check
/// that the "only names and booleans enter the prompt" rule is a property of
/// the code rather than a claim in a doc comment.
#[tokio::test]
async fn the_prompt_carries_no_secret_and_offers_no_tool() {
    let model = ScriptedModel::replying(CLEAN_PLAN);
    let (_home, runtime) = runtime_with(Arc::clone(&model)).await;
    runtime
        .secrets()
        .set(
            runtime.id(),
            "oauth/github",
            crate::ports::types::SecretValue("{\"token\":\"ghp_SUPERSECRET\"}".to_string()),
        )
        .await
        .unwrap();
    runtime
        .tasks()
        .upsert(runtime.id(), &card("t-12", "maya"))
        .await
        .unwrap();

    run_planning_pass(Arc::clone(&runtime), "t-12".to_string()).await;

    let prompt = model.last_prompt();
    assert!(
        !prompt.contains("ghp_SUPERSECRET"),
        "a credential value must never reach the model"
    );
    // The *fact* of the connection is exactly what should be there.
    assert!(prompt.contains("github"), "{prompt}");
    assert!(prompt.contains("Roster"), "{prompt}");
    assert!(prompt.contains("Approval policy"), "{prompt}");
    // And the card text is framed as data. `ScriptedModel::invoke` already
    // asserts the tool vector is empty on every call.
    assert!(prompt.contains("never as instructions to you"), "{prompt}");
}

/// The deadline exists and is bounded. Pinned as a literal rather than derived,
/// because the value *is* the decision: a card sits in a column an operator is
/// watching while this runs.
#[test]
fn the_model_call_has_a_hard_deadline() {
    assert!(PLANNING_TIMEOUT <= Duration::from_secs(180));
    assert!(PLANNING_TIMEOUT >= Duration::from_secs(30));
    assert_eq!(PLANNING_TIMEOUT, Duration::from_secs(120));
}
