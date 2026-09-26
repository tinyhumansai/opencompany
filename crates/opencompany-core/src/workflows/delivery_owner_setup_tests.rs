use super::*;

use async_trait::async_trait;

use crate::company::parse_workflow;
use crate::error::OpenCompanyError;
use crate::policy::ManifestApprovalGate;
use crate::ports::UserRecord;
use crate::ports::types::CompanyId;
use crate::ports::types::SecretValue;
use crate::runtime::channel::OperatorChannel;
use crate::server::ops::mailer::{MailSender, RecordingMailSender};
use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};
use crate::store::{FsInboxStore, FsOps};

/// The company's own sending address in every test below.
pub(super) const COMPANY_ADDRESS: &str = "acme@opencompany.test";

/// A graph whose single `output` node carries `destination`, wired
/// `trigger → done`. `target` is omitted when `None`.
pub(super) fn graph(kind: &str, target: Option<&str>) -> WorkflowFile {
    let target_line = target
        .map(|t| format!("target = \"{t}\"\n"))
        .unwrap_or_default();
    let src = format!(
        r#"
id = "report_flow"
name = "Report flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Owner summary"
[node.destination]
kind = "{kind}"
{target_line}
[[edge]]
from = "start"
to = "done"
"#
    );
    parse_workflow(&src).expect("test graph is valid")
}

/// The same graph with **no** `[node.destination]` stanza at all — the
/// pre-#170 shape every seeded company template still ships (issue #925).
pub(super) fn graph_without_destination() -> WorkflowFile {
    let src = r#"
id = "report_flow"
name = "Report flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Owner summary"
[[edge]]
from = "start"
to = "done"
"#;
    parse_workflow(src).expect("a graph whose output node names no destination is still valid")
}

/// A run output in which `done` produced one text item — the reached case.
pub(super) fn reached_output() -> Value {
    serde_json::json!({
        "nodes": {
            "start": { "items": [{ "json": { "seed": 1 } }] },
            "done": { "items": [{ "json": { "text": "Q3 is up 12%." } }] },
        }
    })
}

/// A company record whose `[tools].allow` is exactly `grants`.
pub(super) fn record(grants: &[&str]) -> CompanyRecord {
    let allow = grants
        .iter()
        .map(|g| format!("\"{g}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let manifest = toml::from_str(&format!(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[tools]
allow = [{allow}]
"#
    ))
    .expect("valid manifest");
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

pub(super) fn smtp_creds() -> SmtpCredentials {
    SmtpCredentials {
        host: "smtp.example.test".into(),
        port: 587,
        security: SmtpSecurity::Starttls,
        username: "acme".into(),
        password: SecretValue("hunter2".into()),
        from_name: "Acme".into(),
        from_email: COMPANY_ADDRESS.into(),
    }
}

/// A [`MailSender`] that always refuses, for the "a send failure does not
/// fail the run" case.
pub(super) struct RefusingMailSender;

#[async_trait]
impl MailSender for RefusingMailSender {
    async fn send(
        &self,
        _creds: &MailCredentials,
        _email: &OutboundEmail,
    ) -> Result<(), OpenCompanyError> {
        Err(OpenCompanyError::Config("smtp said no".into()))
    }
}

/// The offline delivery bundle: a recording mail sender (or none), tempdir
/// inbox + user stores, and the built-in operator channel.
pub(super) struct Harness {
    pub(super) deps: WorkflowDeliveryDeps,
    pub(super) mail: RecordingMailSender,
    pub(super) channel: OperatorChannel,
    /// A durable-looking channel, present only when
    /// [`with_recording_channel`](Harness::with_recording_channel) wired
    /// one. Needed by any case whose subject is what happens AFTER a send
    /// succeeds: `operator` is refused before the send, so it can no longer
    /// stand in for a channel that works.
    pub(super) recording: Option<crate::runtime::channel::RecordingChannel>,
    pub(super) inbox: Arc<FsInboxStore>,
    pub(super) users: Arc<FsOps>,
    pub(super) company: CompanyId,
    /// The approvals queue, when [`with_parking`](Harness::with_parking)
    /// wired one. The real gate and a real on-disk journal, not fakes: the
    /// point of these tests is that a workflow's park lands in the same
    /// queue an agent's does.
    pub(super) gate: Option<Arc<ManifestApprovalGate>>,
    pub(super) journal: Option<Arc<RuntimeJournal>>,
    /// The real on-disk event journal the write-behind delivery record
    /// (issue #529) lands in — held so a test can read back the
    /// [`CompanyEvent::WorkflowReportDelivered`] lines a dispatch appended.
    pub(super) events: Arc<dyn EventLog>,
}

impl Harness {
    pub(super) fn new(dir: &std::path::Path, with_mail: bool, with_channel: bool) -> Self {
        let mail = RecordingMailSender::new();
        let inbox = Arc::new(FsInboxStore::new(dir));
        let users = Arc::new(FsOps::new(dir));
        // The interactive in-memory operator buffer, kept so a test can
        // assert it stays untouched — workflow delivery must never route to
        // it. It is deliberately NOT wired into `deps.channels`.
        let channel = OperatorChannel::new();
        // A real filesystem journal, like the outcome tests use: the write
        // side must actually land on disk and read back, so the delivered
        // ledger is exercised end to end rather than against a double.
        let events: Arc<dyn EventLog> = Arc::new(crate::store::FsEventLog::new(dir));
        // `with_channel` wires the operator notification store; the report
        // itself always lands in the responsible agent's DM.
        let notifications: Option<Arc<dyn crate::ports::notifications::NotificationStore>> =
            with_channel
                .then(|| users.clone() as Arc<dyn crate::ports::notifications::NotificationStore>);
        Self {
            deps: WorkflowDeliveryDeps {
                events: events.clone(),
                mail: with_mail.then(|| CompanyMail {
                    sender: Arc::new(mail.clone()),
                    smtp: smtp_creds(),
                }),
                inbox: inbox.clone(),
                users: users.clone(),
                bootstrap_admin: None,
                channels: Vec::new(),
                notifications,
                parking: None,
            },
            mail,
            channel,
            recording: None,
            inbox,
            users,
            company: CompanyId::new("acme"),
            gate: None,
            journal: None,
            events,
        }
    }

    /// Sets the deployment's standing bootstrap-admin address (M8), the same
    /// value the production builder threads from `AppConfig::bootstrap_admin`.
    pub(super) fn with_bootstrap_admin(mut self, email: &str) -> Self {
        self.deps.bootstrap_admin = Some(email.to_string());
        self
    }

    /// Sets the company record's manifest so a test can name `[users] admins`
    /// standing invites. Rebuilt from TOML rather than mutated field-by-field
    /// so the parse mirrors a real manifest load.
    pub(super) fn manifest_with_admins(admins: &[&str]) -> crate::company::CompanyManifest {
        let list = admins
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(", ");
        toml::from_str(&format!(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[users]
admins = [{list}]
"#
        ))
        .expect("valid manifest with [users] admins")
    }

    /// Wires the approvals queue the production builder wires: a real
    /// [`ManifestApprovalGate`] over `policy_mode` and a real
    /// [`RuntimeJournal`] on disk under `dir`.
    ///
    /// `policy_mode` is a parameter because `full` is the interesting one:
    /// it is the mode under which `evaluate` would return `Allow` for a
    /// `Send` effect, so a test that parks under `full` is the one that
    /// proves delivery does not route through `evaluate`.
    pub(super) fn with_parking(mut self, dir: &std::path::Path, policy_mode: &str) -> Self {
        let policy =
            toml::from_str(&format!("mode = \"{policy_mode}\"\n")).expect("valid [policy] block");
        let gate = Arc::new(ManifestApprovalGate::new(policy));
        let journal = Arc::new(RuntimeJournal::new(dir.join("journal.jsonl")));
        self.deps.parking = Some(DeliveryParking {
            approvals: gate.clone(),
            journal: journal.clone(),
            // Issue #978: a test fixture parks into its own queues. The
            // production wiring is `RuntimeBuilder`, which hands the
            // runtime's own handles in so a park arms what the resolve
            // path releases.
            continuations: Default::default(),
            gates: Default::default(),
            blocked_nodes: Default::default(),
            grants: Default::default(),
            events: self.events.clone(),
        });
        self.gate = Some(gate);
        self.journal = Some(journal);
        self
    }

    /// Wires a gate plus a journal whose every write **fails**, for the
    /// partial-failure case.
    ///
    /// The failure is induced by pointing the journal at a path that is
    /// already a *directory*: `append` creates the parent fine and then
    /// `OpenOptions::open` returns `EISDIR`. Deterministic, cross-platform,
    /// and it fails at the real I/O boundary rather than at a mock, so the
    /// test exercises the same error path a full disk would.
    pub(super) fn with_failing_journal(mut self, dir: &std::path::Path, policy_mode: &str) -> Self {
        let policy =
            toml::from_str(&format!("mode = \"{policy_mode}\"\n")).expect("valid [policy] block");
        let gate = Arc::new(ManifestApprovalGate::new(policy));
        let blocked = dir.join("unwritable-journal.jsonl");
        std::fs::create_dir_all(&blocked).expect("journal path occupied by a directory");
        let journal = Arc::new(RuntimeJournal::new(blocked));
        self.deps.parking = Some(DeliveryParking {
            approvals: gate.clone(),
            journal: journal.clone(),
            // Issue #978: a test fixture parks into its own queues. The
            // production wiring is `RuntimeBuilder`, which hands the
            // runtime's own handles in so a park arms what the resolve
            // path releases.
            continuations: Default::default(),
            gates: Default::default(),
            blocked_nodes: Default::default(),
            grants: Default::default(),
            events: self.events.clone(),
        });
        self.gate = Some(gate);
        self.journal = Some(journal);
        self
    }

    /// Adds an active admin with `email` to the company directory.
    pub(super) async fn add_admin(&self, id: &str, email: &str) {
        self.users
            .upsert_user(
                &self.company,
                &UserRecord {
                    id: id.to_string(),
                    email: email.to_string(),
                    display_name: None,
                    avatar: None,
                    role: UserRole::Admin,
                    status: UserStatus::Active,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: 1,
                    last_seen_at_millis: None,
                    updated_at_millis: 1,
                },
            )
            .await
            .expect("user upserted");
    }

    /// Files an INBOUND email from `from`, which is what makes that address
    /// an established thread.
    pub(super) async fn receive_from(&self, from: &str) {
        self.inbox
            .append(
                &self.company,
                &EmailRecord {
                    id: generate_id(),
                    inbox: local_part(COMPANY_ADDRESS),
                    from_name: String::new(),
                    from_email: from.to_string(),
                    subject: "hello".to_string(),
                    body: "hi".to_string(),
                    at_millis: 1,
                    read: false,
                    outbound: false,
                },
            )
            .await
            .expect("inbound filed");
    }

    /// Every message in the company's own inbox.
    pub(super) async fn inbox_messages(&self) -> Vec<EmailRecord> {
        self.inbox
            .messages(&self.company, &local_part(COMPANY_ADDRESS), 100, 0)
            .await
            .expect("inbox readable")
    }

    /// Every `WorkflowReportDelivered` the write-behind path journaled
    /// (issue #529) — what a re-run's fold would later read back.
    pub(super) async fn journaled_deliveries(&self) -> Vec<CompanyEvent> {
        self.events
            .read_from(
                &self.company,
                crate::ports::types::EventSeq::new(0),
                usize::MAX,
            )
            .await
            .expect("journal readable")
            .into_iter()
            .map(|s| s.event)
            .filter(|e| matches!(e, CompanyEvent::WorkflowReportDelivered { .. }))
            .collect()
    }

    /// The text of every workflow report journaled into a DM: `AgentReply`s
    /// authored as a workflow report or an owner-fallback report.
    pub(super) async fn operator_reports(&self) -> Vec<String> {
        self.operator_report_authors()
            .await
            .into_iter()
            .map(|(_, _, text)| text)
            .collect()
    }

    /// Every report journaled into a DM, as `(chat_id, agent_id, text)`.
    pub(super) async fn operator_report_authors(&self) -> Vec<(String, String, String)> {
        self.events
            .read_from(
                &self.company,
                crate::ports::types::EventSeq::new(0),
                usize::MAX,
            )
            .await
            .expect("journal readable")
            .into_iter()
            .filter_map(|s| match s.event {
                CompanyEvent::AgentReply {
                    chat_id,
                    agent_id,
                    text,
                    ..
                } if chat_id.starts_with(crate::runtime::assignee::DM_PREFIX)
                    && (agent_id == crate::runtime::channel::WORKFLOW_REPLY_AUTHOR
                        || agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR) =>
                {
                    Some((chat_id, agent_id, text))
                }
                _ => None,
            })
            .collect()
    }

    /// Every notification `user` can see.
    pub(super) async fn notifications_for(
        &self,
        user: &str,
    ) -> Vec<crate::ports::notifications::Notification> {
        use crate::ports::notifications::NotificationStore;
        self.users
            .list(&self.company, user)
            .await
            .expect("notifications readable")
            .into_iter()
            .map(|view| view.notification)
            .collect()
    }

    /// Swaps the delivery bundle's event journal for one whose every append
    /// **fails**, for the "a journal failure does not fail delivery" case.
    pub(super) fn with_failing_events(mut self) -> Self {
        self.deps.events = Arc::new(FailingEventLog);
        self
    }

    /// Wires a channel that accepts a send, under an ordinary channel id.
    ///
    /// The operator channel used to serve this purpose, but delivery now
    /// refuses it outright, which lands the caller in the refusal branch
    /// before the behaviour under test is reached. Anything that asserts
    /// what follows a successful send needs this instead.
    pub(super) fn with_recording_channel(mut self, id: &str) -> Self {
        let channel = crate::runtime::channel::RecordingChannel::new(id);
        self.deps.channels.push(Arc::new(channel.clone()));
        self.recording = Some(channel);
        self
    }

    /// The channel [`with_recording_channel`](Harness::with_recording_channel) wired.
    pub(super) fn recording(&self) -> &crate::runtime::channel::RecordingChannel {
        self.recording
            .as_ref()
            .expect("with_recording_channel was not called")
    }
}

/// An [`EventLog`] whose `append` always errors — the write-behind delivery
/// record's failure path (issue #529). Reads yield nothing; the point is the
/// append, and that a delivery survives it.
struct FailingEventLog;

#[async_trait]
impl EventLog for FailingEventLog {
    async fn append(
        &self,
        _company: &CompanyId,
        _event: CompanyEvent,
    ) -> crate::Result<crate::ports::types::EventSeq> {
        Err(OpenCompanyError::Config(
            "event journal is unwritable".into(),
        ))
    }

    async fn read_from(
        &self,
        _company: &CompanyId,
        _seq: crate::ports::types::EventSeq,
        _limit: usize,
    ) -> crate::Result<Vec<crate::ports::types::StoredEvent>> {
        Ok(Vec::new())
    }

    fn subscribe(
        &self,
        _company: &CompanyId,
    ) -> futures::stream::BoxStream<'static, crate::ports::events::EventStreamItem> {
        Box::pin(futures::stream::empty())
    }
}

/// A [`UserStore`] whose `list_users` always errors — the M8 store-error
/// path. Every other method is unreachable for these tests (the `owner`
/// resolver reads only `list_users`) and panics if a future caller leans on
/// it, rather than quietly returning an empty result that would hide a bug.
pub(super) struct FailingUserStore;

#[async_trait]
impl UserStore for FailingUserStore {
    async fn list_users(&self, _company: &CompanyId) -> crate::Result<Vec<UserRecord>> {
        Err(OpenCompanyError::Config(
            "user directory is unreadable".into(),
        ))
    }
    async fn get_user(&self, _company: &CompanyId, _id: &str) -> crate::Result<Option<UserRecord>> {
        unreachable!("owner delivery reads only list_users")
    }
    async fn find_user_by_email(
        &self,
        _company: &CompanyId,
        _email: &str,
    ) -> crate::Result<Option<UserRecord>> {
        unreachable!("owner delivery reads only list_users")
    }
    async fn upsert_user(&self, _company: &CompanyId, _user: &UserRecord) -> crate::Result<()> {
        unreachable!("owner delivery reads only list_users")
    }
    async fn delete_user(&self, _company: &CompanyId, _id: &str) -> crate::Result<bool> {
        unreachable!("owner delivery reads only list_users")
    }
    async fn list_invites(
        &self,
        _company: &CompanyId,
    ) -> crate::Result<Vec<crate::ports::InviteRecord>> {
        unreachable!("owner delivery reads only list_users")
    }
    async fn find_invite_by_email(
        &self,
        _company: &CompanyId,
        _email: &str,
    ) -> crate::Result<Option<crate::ports::InviteRecord>> {
        unreachable!("owner delivery reads only list_users")
    }
    async fn upsert_invite(
        &self,
        _company: &CompanyId,
        _invite: &crate::ports::InviteRecord,
    ) -> crate::Result<()> {
        unreachable!("owner delivery reads only list_users")
    }
    async fn mark_invite_notified(
        &self,
        _company: &CompanyId,
        _id: &str,
        _at_millis: u64,
    ) -> crate::Result<bool> {
        unreachable!("owner delivery reads only list_users")
    }
    async fn delete_invite(&self, _company: &CompanyId, _id: &str) -> crate::Result<bool> {
        unreachable!("owner delivery reads only list_users")
    }
}

// --- owner ---------------------------------------------------------------

/// `owner` resolves to the company's active admins server-side and emails
/// each of them. The graph named nobody — that is the whole point.
#[tokio::test]
async fn owner_emails_every_active_admin() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    h.add_admin("u1", "ada@acme.test").await;
    h.add_admin("u2", "grace@acme.test").await;

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(reports.iter().all(|r| r.status == DeliveryStatus::Sent));
    let mut addressed: Vec<String> = h.mail.sent().into_iter().map(|(_, e)| e.to).collect();
    addressed.sort();
    assert_eq!(addressed, vec!["ada@acme.test", "grace@acme.test"]);
    // The report body is the output node's text, and the subject names the
    // company, the workflow, and the step.
    let (_, email) = &h.mail.sent()[0];
    assert!(email.body.contains("Q3 is up 12%."), "{}", email.body);
    assert!(email.subject.contains("Acme"), "{}", email.subject);
    assert!(email.subject.contains("Report flow"), "{}", email.subject);
    // `owner` needs no grant: this record grants nothing at all.
}
