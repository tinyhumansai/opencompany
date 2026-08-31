//! [`LifecycleScheduler`]: the week-1 "save your first workflow" nudge
//! (issue #1845).
//!
//! Part of the OpenCompany pre-launch retention strategy's fix for the 14%
//! churn cause "never saved a workflow": a signup who never saves one has
//! nothing to come back to. This scheduler is what turns that observation
//! into an actual nudge — one email plus one durable in-app notification, at
//! most once per user, at the day-7 boundary after signup.
//!
//! # Shaped like the existing schedulers, on purpose
//!
//! Modeled on [`WorkflowScheduler`](super::WorkflowScheduler) and
//! [`CompanyScheduler`](super::scheduler::CompanyScheduler): one
//! process-global task (not one per company — a hosted tenant can be
//! registered after boot, same reasoning as `WorkflowScheduler`'s own docs),
//! an injectable [`Clock`], the same tick-then-sleep loop shape. What differs
//! is what "due" means: there is no cron expression to match. [`Self::tick`]
//! walks every registered company's users once and, for each user past their
//! day-7 boundary, decides — once — whether they earned a nudge.
//!
//! # The idempotency ledger IS the notification row
//!
//! There is no separate "who have we nudged" table. [`Self::tick`] asks
//! [`NotificationStore::list`](crate::ports::notifications::NotificationStore::list)
//! for this user's own notifications and checks whether one already carries
//! [`week1_nudge::NUDGE_KIND`](crate::company::week1_nudge::NUDGE_KIND); if
//! so, the decision was already made (nudged, or the row would not exist) and
//! this tick does nothing further for them. This is a **best-effort**
//! check-then-act, not a durable claim — unlike
//! [`ScheduleFireStore::claim_fire`](crate::ports::ScheduleFireStore::claim_fire),
//! `NotificationStore` has no compare-and-swap primitive. Two REPLICAS
//! ticking at the same instant for the same user could theoretically both
//! pass the check and both file a row (and, worst case, both send an email).
//! Accepted for v1: a single duplicate nudge is a mildly annoying email, not
//! a correctness defect, and this is the same bar the issue's own idempotency
//! ask sets ("use a `NotificationStore` row as the idempotency ledger") — a
//! durable cross-replica claim would need a new store primitive this issue
//! does not scope.
//!
//! # The deploy cutoff sidesteps the attribution gap
//!
//! `WorkflowCreated.by` was `None` on every create path before issue #1843.
//! A user who signed up before this scheduler's process started may have
//! saved a workflow through one of those unattributed paths, and there is no
//! way to tell that user apart from one who truly never saved anything — so
//! this scheduler never nudges them at all, rather than risk a false-positive
//! nag. [`Self::cutoff_millis`] is stamped once — the first time
//! [`load_or_create_cutoff_millis`] ever runs against a given data root, not
//! re-stamped on every boot — and only users created at or after it are ever
//! considered. It has to be pinned rather than re-derived from "now" at each
//! boot: a deploy restarts the process, and re-stamping would move the
//! cutoff forward on every restart, permanently disqualifying anyone who
//! signed up in between. See [`crate::company::week1_nudge`]'s module docs
//! for the same attribution gap from the query's side.
//!
//! # Email is primary, in-app is the substrate that always lands
//!
//! The in-app [`Notification`] row is written **first**, unconditionally —
//! it is both the idempotency ledger and the durable half issue #1845 scope
//! item 4 asks for. Email is attempted only after that row exists, exactly
//! the transport [`crate::server::users::routes::deliver_code`] sends the
//! login link through: host-level [`MailSender`] + [`MailCredentials`]
//! (`OPENCOMPANY_MAIL_*`, wired only when the binary is built with the
//! `smtp` feature). Missing either — no feature, no host mail configured, or
//! a user whose login identity carries no mailbox (wallet/local auth) —
//! degrades LOUDLY (one `info!` line naming which reason) rather than
//! panicking or silently trying to send. Email failing at the transport
//! (`MailSender::send` returning `Err`) is logged and swallowed the same way
//! `deliver_code`'s login mail is: the in-app row already landed, so the
//! nudge is not lost, only quieter than it should have been.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::company::runtime::CompanyRuntime;
use crate::company::week1_nudge::{NUDGE_KIND, SEVEN_DAYS_MILLIS, user_saved_workflow_in_week1};
use crate::ports::notifications::{Notification, Subject, SubjectKind};
use crate::ports::now_millis;
use crate::ports::types::CompanyId;
use crate::ports::users::{UserRecord, UserStatus};
use crate::runtime::CompanyRegistry;
use crate::runtime::scheduler::Clock;
use crate::server::ops::mailer::{MailCredentials, MailSender, OutboundEmail};

/// How far past the day-7 boundary a tick still attempts a nudge.
///
/// Bounds the daily scan the same way
/// [`CATCHUP_WINDOW_MINUTES`](super::scheduler::CATCHUP_WINDOW_MINUTES)
/// bounds the cron schedulers' restart catch-up: without a ceiling, a user
/// who is neither nudged nor activated would be re-evaluated on every tick
/// for the rest of the process's life. Fourteen days — twice the nudge
/// window itself — is generous enough that a daily tick can never miss the
/// boundary, while still being a bound.
const LOOKBACK_MILLIS: u64 = 14 * 24 * 60 * 60 * 1000;

/// How often [`LifecycleScheduler::spawn`] ticks in production. A daily cron
/// per the issue's own spec — this is a day-granularity decision ("has a
/// week passed"), not a minute-granularity one, so it needs none of the
/// per-minute matching the cron schedulers do.
const TICK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The file under the host's data root that pins the deploy cutoff.
const CUTOFF_FILE: &str = "week1-nudge-cutoff";

/// Reads the deploy cutoff for `home`, minting and persisting one on first
/// use.
///
/// The module docs' "deploy cutoff" section explains why a cutoff exists;
/// this is why it now survives a restart. Before this, the production caller
/// passed `now_millis()` straight into [`LifecycleScheduler::new`] on every
/// boot, so a restart moved the cutoff forward to that boot's instant —  and
/// [`LifecycleScheduler::tick`] treats "signed up before the cutoff" as
/// unanswerable and never nudges that user again, for the rest of their
/// account's life. A deploy restarting mid-week for an already-eligible
/// signup therefore permanently disqualified them. Pinning the value the
/// first time this scheduler ever runs against a given `home`, the same way
/// [`crate::app::instance::load_or_create`] pins the host's instance id,
/// makes every later boot reuse it instead of moving the goalposts.
///
/// Never fails: an unwritable `home` mints a fresh value for this process and
/// logs the degradation rather than aborting boot over a nudge timestamp —
/// the same trade-off `instance::load_or_create` makes for the same reason.
pub fn load_or_create_cutoff_millis(home: &std::path::Path) -> u64 {
    let path = home.join(CUTOFF_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if let Ok(parsed) = existing.trim().parse::<u64>() {
            return parsed;
        }
        tracing::warn!(
            path = %path.display(),
            "week1-nudge-cutoff file is not a well-formed timestamp; minting a replacement"
        );
    }
    let minted = now_millis();
    if let Err(error) = std::fs::write(&path, minted.to_string()) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "could not persist the week-1 nudge cutoff; every restart before this is fixed \
             will move the cutoff forward and can permanently exclude eligible signups"
        );
    }
    minted
}

/// The [`Subject::id`] every week-1 nudge notification carries.
///
/// Not a real workflow id — there is no workflow yet, which is the entire
/// point of the nudge — but [`Subject`] is a closed `{kind, id}` pair and
/// [`SubjectKind::Workflow`] is the closest existing tag for "this is about
/// creating one". A fixed constant rather than the user or company id:
/// [`Notification::audience`] and the company scope already carry those, so
/// reusing either here would make `id` redundant with a field the row
/// already has.
const NUDGE_SUBJECT_ID: &str = "week1-first-workflow";

/// Drives the week-1 "save your first workflow" nudge across every
/// registered company. See the module docs for the shape and the guarantees.
pub struct LifecycleScheduler {
    registry: CompanyRegistry,
    clock: Arc<dyn Clock>,
    /// Host-level mail sender (real under the `smtp` feature; `None` in the
    /// default build or when no host mail is configured). Absence degrades
    /// the nudge to in-app only — see the module docs.
    mail: Option<Arc<dyn MailSender>>,
    /// Host-level mail credentials (`OPENCOMPANY_MAIL_*`) — platform mail,
    /// the same scope the login-link email is sent under, never a company's
    /// own SMTP secret.
    mail_credentials: Option<MailCredentials>,
    /// The base URL the nudge email's link is built against
    /// ([`AppConfig::host_base_url`](crate::app::types::AppConfig::host_base_url)).
    host_base_url: String,
    /// Only a user created at or after this instant is ever considered — see
    /// the module docs' "deploy cutoff" section.
    cutoff_millis: u64,
}

impl LifecycleScheduler {
    /// Builds a scheduler over every company in `registry`, driven by
    /// `clock`. `cutoff_millis` is normally
    /// [`load_or_create_cutoff_millis`] read once at process boot — a value
    /// pinned to the host's data root the first time this scheduler ever
    /// runs, not `clock.now_millis()` recomputed on every boot (see that
    /// function's docs for why); tests pass a fixed value so a seeded user
    /// can be placed on either side of it.
    pub fn new(
        registry: CompanyRegistry,
        clock: Arc<dyn Clock>,
        mail: Option<Arc<dyn MailSender>>,
        mail_credentials: Option<MailCredentials>,
        host_base_url: String,
        cutoff_millis: u64,
    ) -> Self {
        Self {
            registry,
            clock,
            mail,
            mail_credentials,
            host_base_url,
            cutoff_millis,
        }
    }

    /// Runs one tick: for every registered, running company, walks its
    /// users and nudges each one that is due. Returns how many nudges were
    /// dispatched.
    ///
    /// A company whose `ensure_running` guard rejects (paused or archived)
    /// contributes nothing this tick — the same skip
    /// [`WorkflowScheduler::tick`](super::WorkflowScheduler::tick) makes, so
    /// a paused company's users simply wait for the next tick after resume
    /// rather than being nudged while nobody would see it land.
    pub async fn tick(&mut self) -> usize {
        let now = self.clock.now_millis();
        let mut nudged = 0;
        for company in self.registry.list() {
            let Some(runtime) = self.registry.get(&company) else {
                continue; // removed between listing and lookup
            };
            if runtime.ensure_running().await.is_err() {
                continue;
            }
            let users = match runtime.users().list_users(&company).await {
                Ok(users) => users,
                Err(err) => {
                    tracing::warn!(
                        %company,
                        %err,
                        "lifecycle scheduler: could not list users; skipping this company this tick"
                    );
                    continue;
                }
            };
            for user in users {
                if user.status != UserStatus::Active {
                    // Retained for attribution but refused at login and on
                    // every request (`UserStatus::Suspended`'s own docs) —
                    // the same bar `workflows::delivery`'s admin-recipient
                    // filter and `server::ops::mentions`'s advertised-user
                    // filter both hold notification recipients to. A
                    // suspended user can neither read the in-app row nor
                    // act on the email, so nudging them is pure noise.
                    continue;
                }
                if user.created_at_millis < self.cutoff_millis {
                    // Pre-deploy signup: the attribution gap makes "never
                    // saved a workflow" unanswerable for them. Never nudge.
                    continue;
                }
                let elapsed = now.saturating_sub(user.created_at_millis);
                if elapsed < SEVEN_DAYS_MILLIS {
                    continue; // not due yet
                }
                if elapsed >= SEVEN_DAYS_MILLIS + LOOKBACK_MILLIS {
                    continue; // past the bounded catch-up window; see LOOKBACK_MILLIS
                }
                match self.maybe_nudge(&company, &runtime, &user, now).await {
                    Ok(true) => nudged += 1,
                    Ok(false) => {}
                    Err(err) => {
                        tracing::warn!(
                            %company,
                            user = %user.id,
                            %err,
                            "lifecycle scheduler: week-1 nudge check failed for this user"
                        );
                    }
                }
            }
        }
        nudged
    }

    /// Decides and, if owed, dispatches one user's week-1 nudge. Returns
    /// whether a nudge was actually filed.
    ///
    /// `now` is this tick's own evaluation instant (from [`Self::tick`]'s
    /// `self.clock.now_millis()`) — passed through to
    /// [`user_saved_workflow_in_week1`] so a save that lands after the
    /// nominal week-1 window but before this tick actually runs still
    /// suppresses the nudge; see that function's docs.
    async fn maybe_nudge(
        &self,
        company: &CompanyId,
        runtime: &CompanyRuntime,
        user: &UserRecord,
        now: u64,
    ) -> crate::Result<bool> {
        // The idempotency ledger: has this user already been nudged?
        let existing = runtime.notifications().list(company, &user.id).await?;
        if existing
            .iter()
            .any(|view| view.notification.kind == NUDGE_KIND)
        {
            return Ok(false);
        }
        // codex review finding (comment 3892534913): `now` is `tick`'s
        // single process-wide instant, captured once at the top and shared
        // across every company and user this loop walks — deliberately so
        // for `elapsed`'s day-7 boundary math and this notification's own
        // `created_at` (see `8912e48d8`, which fixed the SAME staleness for
        // `created_at` specifically). But `EventLog::append` always stamps a
        // journaled event with the real wall clock (`crate::ports::now_millis`,
        // never `self.clock` — see this test module's own `real_now` doc), so
        // a workflow saved by this user after `now` was captured but before
        // this iteration reaches them journals with a timestamp LATER than
        // the frozen `now`, and the completeness check below would reject an
        // event that, by the actual instant this code runs, has already
        // happened. A freshly-read real clock — not `now`, not `self.clock`,
        // which in tests is a `FakeClock` decoupled from the journal's real
        // timestamps entirely — is what this specific comparison needs: it is
        // checking against journal timestamps that are always real time,
        // regardless of what clock the scheduler itself is running on.
        if user_saved_workflow_in_week1(
            company,
            runtime.events(),
            &user.id,
            user.created_at_millis,
            crate::ports::now_millis(),
        )
        .await?
        {
            return Ok(false); // earned activation by the time this tick ran: no nudge owed
        }

        // The in-app row lands FIRST and unconditionally — it is both the
        // ledger a later tick reads and scope item 4's own substrate. Email
        // is attempted only once this exists.
        let notification = Notification {
            id: crate::ports::generate_id(),
            kind: NUDGE_KIND.to_string(),
            subject: Subject {
                kind: SubjectKind::Workflow,
                id: NUDGE_SUBJECT_ID.to_string(),
            },
            created_at: now,
            title: "Save your first workflow".to_string(),
            audience: Some(vec![user.id.clone()]),
            context: None,
        };
        runtime
            .notifications()
            .append(company, &notification)
            .await?;

        self.send_email(company, runtime, user).await;

        Ok(true)
    }

    /// Best-effort email delivery for one nudge. Never returns an error —
    /// every refusal reason (no transport, no mailbox, transport failure) is
    /// logged and swallowed, because the in-app row filed by
    /// [`Self::maybe_nudge`] already makes the nudge durable; email is the
    /// primary *reach*, not the primary *record*.
    async fn send_email(&self, company: &CompanyId, runtime: &CompanyRuntime, user: &UserRecord) {
        let (Some(sender), Some(creds)) = (&self.mail, &self.mail_credentials) else {
            // Loud, per the issue's own "if smtp absent, degrade LOUDLY"
            // instruction — not merely a debug line nobody sees.
            tracing::info!(
                %company,
                user = %user.id,
                "lifecycle scheduler: no host mail transport wired (OPENCOMPANY_MAIL_* / \
                 `smtp` feature); week-1 nudge for this user stays in-app only"
            );
            return;
        };
        let Some(mailbox) = user.mailbox() else {
            tracing::info!(
                %company,
                user = %user.id,
                "lifecycle scheduler: user's login identity has no mailbox (wallet/local \
                 auth); week-1 nudge stays in-app only"
            );
            return;
        };
        let company_name = match runtime.store().load(company).await {
            Ok(Some(record)) => record.manifest.company.name,
            _ => company.as_ref().to_string(),
        };
        // Same shape as the login link (`deliver_code` /
        // `server::users::admin`'s invite mail): land on sign-in, carrying the
        // console's own `#/workflows` fragment so a signed-in click goes
        // straight to the empty state's "Create a workflow" CTA rather than
        // wherever the console last had them.
        let link = format!(
            "{}/login?company={}#/workflows",
            self.host_base_url,
            company.as_ref()
        );
        let mail = OutboundEmail {
            to: mailbox,
            subject: format!("Save your first workflow in {company_name}"),
            body: format!(
                "You joined {company_name} about a week ago and haven't saved a workflow \
                 yet — that's the thing {company_name} actually runs on a schedule or on \
                 demand, once you've described it.\n\n\
                 Describe one in plain words and the copilot drafts the graph for you to \
                 review:\n\n{link}\n\n\
                 If you've already got one you're happy with, there's nothing else to do — \
                 you won't hear about this again.\n"
            ),
        };
        if let Err(err) = sender.send(creds, &mail).await {
            // The error, never the message: consistent with `deliver_code`'s
            // own login mail — the address itself must never reach a log
            // line via an interpolated `detail`.
            tracing::warn!(
                %company,
                user = %user.id,
                "lifecycle scheduler: week-1 nudge email failed: {err}"
            );
        }
    }

    /// Spawns a background task that ticks once immediately, then every
    /// [`TICK_INTERVAL`], until `shutdown` is notified.
    ///
    /// Ticking once before the loop (rather than waiting a full day for the
    /// first pass) matters here specifically: at boot there may already be
    /// users well past their day-7 boundary, and making every one of them
    /// wait up to 24h for a scheduler that only just started would be its
    /// own small defect.
    pub fn spawn(mut self, shutdown: Arc<Notify>) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.tick().await;
            let notified = shutdown.notified();
            tokio::pin!(notified);
            loop {
                tokio::select! {
                    _ = &mut notified => break,
                    _ = tokio::time::sleep(TICK_INTERVAL) => {
                        self.tick().await;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod test {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::company::CompanyManifest;
    use crate::error::OpenCompanyError;
    use crate::ports::types::{Actor, ActorKind, CompanyEvent};
    use crate::ports::users::{UserRole, UserStatus};
    use crate::runtime::{FakeClock, RuntimeBuilder};
    use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};

    fn tmp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-lifecycle-")
            .tempdir()
            .expect("tempdir")
    }

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n")
            .expect("valid manifest")
    }

    async fn seed_user(runtime: &CompanyRuntime, id: &CompanyId, uid: &str, created_at: u64) {
        runtime
            .users()
            .upsert_user(
                id,
                &UserRecord {
                    id: uid.to_string(),
                    email: format!("{uid}@example.test"),
                    display_name: None,
                    avatar: None,
                    role: UserRole::Member,
                    status: UserStatus::Active,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: created_at,
                    last_seen_at_millis: None,
                    updated_at_millis: created_at,
                },
            )
            .await
            .expect("seed user");
    }

    async fn seed_suspended_user(
        runtime: &CompanyRuntime,
        id: &CompanyId,
        uid: &str,
        created_at: u64,
    ) {
        runtime
            .users()
            .upsert_user(
                id,
                &UserRecord {
                    id: uid.to_string(),
                    email: format!("{uid}@example.test"),
                    display_name: None,
                    avatar: None,
                    role: UserRole::Member,
                    status: UserStatus::Suspended,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: created_at,
                    last_seen_at_millis: None,
                    updated_at_millis: created_at,
                },
            )
            .await
            .expect("seed suspended user");
    }

    async fn create_workflow_for(runtime: &CompanyRuntime, id: &CompanyId, uid: &str) {
        runtime
            .events()
            .append(
                id,
                CompanyEvent::WorkflowCreated {
                    workflow_id: "wf-1".to_string(),
                    name: "My workflow".to_string(),
                    by: Some(Actor {
                        kind: ActorKind::User,
                        id: uid.to_string(),
                    }),
                },
            )
            .await
            .expect("journal create");
    }

    /// A recording [`MailSender`] double: never touches the network, records
    /// every send.
    struct RecordingMail {
        sent: Mutex<Vec<OutboundEmail>>,
        fail: bool,
    }

    impl RecordingMail {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                fail: false,
            })
        }
        fn failing() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                fail: true,
            })
        }
    }

    #[async_trait]
    impl MailSender for RecordingMail {
        async fn send(
            &self,
            _creds: &MailCredentials,
            email: &OutboundEmail,
        ) -> Result<(), OpenCompanyError> {
            if self.fail {
                return Err(OpenCompanyError::Config("mail refused".to_string()));
            }
            self.sent.lock().unwrap().push(email.clone());
            Ok(())
        }
    }

    fn creds() -> MailCredentials {
        MailCredentials::Smtp(SmtpCredentials {
            host: "smtp.example.test".to_string(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "user".to_string(),
            password: crate::ports::types::SecretValue("secret".to_string()),
            from_name: "OpenCompany".to_string(),
            from_email: "nudge@example.test".to_string(),
        })
    }

    const SEVEN_DAYS: u64 = SEVEN_DAYS_MILLIS;

    /// A real "now" for anchoring signup/clock offsets in these tests.
    ///
    /// `EventLog::append` always stamps `at_millis` with the real wall clock
    /// (`crate::ports::now_millis`) — it is not driven by the [`FakeClock`]
    /// injected into the scheduler, which only stands in for the scheduler's
    /// own "what time is it" question. A test that journals a
    /// `WorkflowCreated` (via [`create_workflow_for`]) therefore has to anchor
    /// its `created_at_millis` / clock offsets to THIS, not to an arbitrary
    /// small epoch like `0` — otherwise the journaled event's real timestamp
    /// falls nowhere near the test's small-number "week-1 window" and the
    /// activation query answers `false` for a create that, by the test's own
    /// story, happened well inside the window.
    fn real_now() -> u64 {
        crate::ports::now_millis()
    }

    async fn scheduler_with_mail(
        home: &std::path::Path,
        manifest: CompanyManifest,
        mail: Arc<RecordingMail>,
        cutoff_millis: u64,
        clock_millis: u64,
    ) -> (LifecycleScheduler, Arc<CompanyRuntime>, CompanyId) {
        let rt = Arc::new(
            RuntimeBuilder::new(home.to_path_buf(), manifest)
                .build()
                .await
                .unwrap(),
        );
        let id = rt.id().clone();
        let registry = CompanyRegistry::new();
        registry.insert(id.clone(), rt.clone());
        let clock = Arc::new(FakeClock::new(clock_millis));
        let scheduler = LifecycleScheduler::new(
            registry,
            clock,
            Some(mail as Arc<dyn MailSender>),
            Some(creds()),
            "https://acme.example".to_string(),
            cutoff_millis,
        );
        (scheduler, rt, id)
    }

    #[test]
    fn cutoff_survives_a_simulated_restart() {
        // Before the fix, the production caller passed `now_millis()`
        // straight into `LifecycleScheduler::new` on every boot, so a
        // restart moved the cutoff forward. `load_or_create_cutoff_millis`
        // is the fix: two "boots" against the same home must agree.
        let home = tmp_home();
        let first_boot = load_or_create_cutoff_millis(home.path());
        // A real restart would also have `now_millis()` tick forward, but
        // the bug this guards is exactly that a *later* value would win if
        // re-minted — so proving equality (not merely "close") is the point.
        let second_boot = load_or_create_cutoff_millis(home.path());
        assert_eq!(
            first_boot, second_boot,
            "the cutoff must be pinned on first use and reused on every later boot, \
             not re-derived from `now` each time"
        );
    }

    #[test]
    fn cutoff_persists_to_disk_and_survives_a_fresh_process_view() {
        // A stronger version of the above: read the persisted value back
        // with a completely independent call (as a real second process
        // would), not just a second in-process call.
        let home = tmp_home();
        let minted = load_or_create_cutoff_millis(home.path());
        let path = home.path().join("week1-nudge-cutoff");
        let on_disk: u64 = std::fs::read_to_string(&path)
            .expect("cutoff file must exist after first use")
            .trim()
            .parse()
            .expect("cutoff file must hold a plain integer");
        assert_eq!(on_disk, minted);
    }

    #[tokio::test]
    async fn a_silent_signup_past_day_seven_gets_nudged_once() {
        let home = tmp_home();
        let mail = RecordingMail::new();
        let signup = real_now();
        let now = signup + SEVEN_DAYS; // exactly the boundary
        let (mut scheduler, rt, id) =
            scheduler_with_mail(home.path(), manifest(), mail.clone(), 0, now).await;
        seed_user(&rt, &id, "user-1", signup).await;

        assert_eq!(scheduler.tick().await, 1, "one nudge dispatched");
        assert_eq!(mail.sent.lock().unwrap().len(), 1, "email sent once");

        let feed = rt.notifications().list(&id, "user-1").await.unwrap();
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0].notification.kind, NUDGE_KIND);
        assert_eq!(
            feed[0].notification.audience.as_deref(),
            Some(&["user-1".to_string()][..])
        );
    }

    /// PR #1878 review finding: `maybe_nudge` stamps the notification's
    /// `created_at` with the real wall clock (`crate::ports::now_millis()`)
    /// instead of `now` — this tick's own evaluation instant, already
    /// threaded in from `self.clock.now_millis()` for exactly this reason.
    /// `a_silent_signup_past_day_seven_gets_nudged_once` never catches this
    /// because its `FakeClock` happens to be parked near real wall-clock time
    /// (`real_now() + SEVEN_DAYS`); this test parks the fake clock far from
    /// real time instead, so a wrong-clock stamp cannot hide behind the two
    /// values coincidentally agreeing.
    #[tokio::test]
    async fn notification_created_at_uses_the_injected_clock_not_the_wall_clock() {
        let home = tmp_home();
        let mail = RecordingMail::new();
        // 1970-01-12 — nowhere near the real wall clock at test-run time.
        let fake_signup: u64 = 1_000_000_000;
        let fake_now = fake_signup + SEVEN_DAYS;
        let (mut scheduler, rt, id) =
            scheduler_with_mail(home.path(), manifest(), mail.clone(), 0, fake_now).await;
        seed_user(&rt, &id, "user-1", fake_signup).await;

        assert_eq!(scheduler.tick().await, 1, "one nudge dispatched");

        let feed = rt.notifications().list(&id, "user-1").await.unwrap();
        assert_eq!(feed.len(), 1);
        assert_eq!(
            feed[0].notification.created_at, fake_now,
            "created_at must come from the tick's own injected-clock instant, \
             not the real wall clock"
        );
    }

    #[tokio::test]
    async fn a_second_tick_never_double_nudges() {
        let home = tmp_home();
        let mail = RecordingMail::new();
        let signup = real_now();
        let (mut scheduler, rt, id) = scheduler_with_mail(
            home.path(),
            manifest(),
            mail.clone(),
            0,
            signup + SEVEN_DAYS,
        )
        .await;
        seed_user(&rt, &id, "user-1", signup).await;

        assert_eq!(scheduler.tick().await, 1);
        assert_eq!(
            scheduler.tick().await,
            0,
            "the second tick finds the ledger row"
        );
        assert_eq!(
            mail.sent.lock().unwrap().len(),
            1,
            "exactly one email, not two"
        );

        let feed = rt.notifications().list(&id, "user-1").await.unwrap();
        assert_eq!(feed.len(), 1, "no duplicate row");
    }

    #[tokio::test]
    async fn a_user_who_saved_shortly_after_signup_is_never_nudged() {
        let home = tmp_home();
        let mail = RecordingMail::new();
        let signup = real_now();
        let (mut scheduler, rt, id) = scheduler_with_mail(
            home.path(),
            manifest(),
            mail.clone(),
            0,
            signup + SEVEN_DAYS,
        )
        .await;
        seed_user(&rt, &id, "user-1", signup).await;
        // Journaled at the real wall clock (see `real_now`'s docs) — a few
        // milliseconds after `signup`, which is still well inside the week-1
        // window `[signup, signup + 7d)`.
        create_workflow_for(&rt, &id, "user-1").await;

        assert_eq!(
            scheduler.tick().await,
            0,
            "activated inside the window: no nudge"
        );
        assert!(mail.sent.lock().unwrap().is_empty());
        let feed = rt.notifications().list(&id, "user-1").await.unwrap();
        assert!(
            feed.is_empty(),
            "no ledger row for a user who never needed one"
        );
    }

    /// codex review finding (comment 3892534913): `tick` captures its single
    /// process-wide `now` ONCE, at the very top, and threads that SAME value
    /// into `maybe_nudge` -> `user_saved_workflow_in_week1`'s
    /// `evaluated_at_millis` for every company and every user it walks. A
    /// workflow saved for THIS user after `now` was captured but before this
    /// user's own turn in the loop is journaled with a REAL wall-clock
    /// timestamp (`EventLog::append` always stamps `crate::ports::now_millis`
    /// — see `real_now`'s own doc) that lands AFTER the frozen `now`, so the
    /// completeness check (`entry.at_millis <= evaluated_at_millis`) rejects
    /// an event that, by the time this tick actually reaches the user, has
    /// already happened. This test pins the injected clock to the exact
    /// instant `create_workflow_for` is about to journal past — the fake
    /// clock never advances on its own, so anything appended afterward reads
    /// as "too late" under the frozen value, reproducing the tick-wide-`now`
    /// staleness `8912e48d8` already fixed for `created_at` but not for this
    /// eligibility check.
    #[tokio::test]
    async fn a_workflow_saved_during_the_tick_still_counts() {
        let home = tmp_home();
        let mail = RecordingMail::new();
        let anchor = real_now();
        let signup = anchor - SEVEN_DAYS - 5_000;
        let (mut scheduler, rt, id) =
            scheduler_with_mail(home.path(), manifest(), mail.clone(), 0, anchor).await;
        seed_user(&rt, &id, "user-1", signup).await;
        // Journaled AFTER `anchor` was captured — `EventLog::append` stamps
        // the real wall clock, which has moved on by the time this line
        // runs, so its `at_millis` is strictly greater than `anchor` (the
        // scheduler's frozen `now`).
        create_workflow_for(&rt, &id, "user-1").await;

        assert_eq!(
            scheduler.tick().await,
            0,
            "the user saved a workflow before this tick actually reached them; \
             must not be nudged just because the save landed after the tick's \
             own frozen `now` was captured"
        );
        assert!(
            mail.sent.lock().unwrap().is_empty(),
            "must not have emailed a user who already saved a workflow"
        );
    }

    #[tokio::test]
    async fn not_yet_due_is_left_alone() {
        let home = tmp_home();
        let mail = RecordingMail::new();
        let signup = real_now();
        // Only three days old.
        let (mut scheduler, rt, id) = scheduler_with_mail(
            home.path(),
            manifest(),
            mail.clone(),
            0,
            signup + 3 * 24 * 60 * 60 * 1000,
        )
        .await;
        seed_user(&rt, &id, "user-1", signup).await;

        assert_eq!(scheduler.tick().await, 0);
        assert!(mail.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_pre_deploy_signup_is_never_nudged() {
        // The attribution-gap sidestep: cutoff is AFTER this user's signup.
        let home = tmp_home();
        let mail = RecordingMail::new();
        let signup = real_now();
        let cutoff = signup + 1_000_000;
        let (mut scheduler, rt, id) = scheduler_with_mail(
            home.path(),
            manifest(),
            mail.clone(),
            cutoff,
            cutoff + SEVEN_DAYS,
        )
        .await;
        seed_user(&rt, &id, "user-1", signup).await; // signed up before the cutoff

        assert_eq!(
            scheduler.tick().await,
            0,
            "pre-deploy signups are never nudged"
        );
        assert!(mail.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_suspended_user_past_day_seven_is_never_nudged() {
        // UserStatus::Suspended: "retained for attribution, but refused at
        // login and on every request" — a suspended user can neither read
        // the in-app row nor act on the email, so a due-for-nudge suspended
        // user must be skipped, not emailed and notified into a void.
        let home = tmp_home();
        let mail = RecordingMail::new();
        let signup = real_now();
        let (mut scheduler, rt, id) = scheduler_with_mail(
            home.path(),
            manifest(),
            mail.clone(),
            0,
            signup + SEVEN_DAYS,
        )
        .await;
        seed_suspended_user(&rt, &id, "user-1", signup).await;

        assert_eq!(
            scheduler.tick().await,
            0,
            "a suspended user must never be nudged"
        );
        assert!(mail.sent.lock().unwrap().is_empty());
        let feed = rt.notifications().list(&id, "user-1").await.unwrap();
        assert!(
            feed.is_empty(),
            "no in-app row should be filed for a suspended user either"
        );
    }

    #[tokio::test]
    async fn past_the_lookback_window_is_left_alone() {
        let home = tmp_home();
        let mail = RecordingMail::new();
        let signup = real_now();
        let far_future = signup + SEVEN_DAYS + LOOKBACK_MILLIS + 1;
        let (mut scheduler, rt, id) =
            scheduler_with_mail(home.path(), manifest(), mail.clone(), 0, far_future).await;
        seed_user(&rt, &id, "user-1", signup).await;

        assert_eq!(
            scheduler.tick().await,
            0,
            "outside the bounded catch-up window"
        );
    }

    #[tokio::test]
    async fn no_mail_transport_still_files_the_in_app_row() {
        let home = tmp_home();
        let rt = Arc::new(
            RuntimeBuilder::new(home.path().to_path_buf(), manifest())
                .build()
                .await
                .unwrap(),
        );
        let id = rt.id().clone();
        let registry = CompanyRegistry::new();
        registry.insert(id.clone(), rt.clone());
        let clock = Arc::new(FakeClock::new(SEVEN_DAYS));
        // No mail sender, no credentials: the `smtp`-absent degradation path.
        let mut scheduler = LifecycleScheduler::new(
            registry,
            clock,
            None,
            None,
            "https://acme.example".to_string(),
            0,
        );
        seed_user(&rt, &id, "user-1", 0).await;

        assert_eq!(
            scheduler.tick().await,
            1,
            "the in-app row is still filed with no transport wired"
        );
        let feed = rt.notifications().list(&id, "user-1").await.unwrap();
        assert_eq!(feed.len(), 1);
    }

    #[tokio::test]
    async fn a_failing_transport_does_not_lose_the_in_app_row_or_panic() {
        let home = tmp_home();
        let mail = RecordingMail::failing();
        let (mut scheduler, rt, id) =
            scheduler_with_mail(home.path(), manifest(), mail.clone(), 0, SEVEN_DAYS).await;
        seed_user(&rt, &id, "user-1", 0).await;

        // Must not panic, and the row must still land even though the send
        // failed.
        assert_eq!(scheduler.tick().await, 1);
        let feed = rt.notifications().list(&id, "user-1").await.unwrap();
        assert_eq!(feed.len(), 1);
    }
}
