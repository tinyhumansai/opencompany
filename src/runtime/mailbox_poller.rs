//! Per-company IMAP poller. Structured like [`CompanyScheduler`]
//! (`crate::runtime::scheduler`): an injectable interval loop that, per tick,
//! fetches new mail via a [`MailReceiver`] and files it through the shared
//! [`file_and_notify`]. Skips while the company is asleep (scale-to-zero:
//! unseen mail waits in Stalwart and is picked up on wake).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::company::runtime::CompanyRuntime;
use crate::ports::inbox::EmailRecord;
use crate::ports::{generate_id, now_millis};
use crate::server::ops::imap::ImapCredentials;
use crate::server::ops::inbox::file_and_notify;
use crate::server::ops::mailer::MailReceiver;
use crate::server::ops::smtp::local_part;

/// Drives one company's IMAP mailbox: on each tick, fetches new mail and files
/// it into the addressed inbox via [`file_and_notify`].
pub struct MailboxPoller {
    runtime: Arc<CompanyRuntime>,
    receiver: Arc<dyn MailReceiver>,
    creds: ImapCredentials,
    address: String,
    interval: Duration,
}

impl MailboxPoller {
    /// Binds a poller to `runtime`'s mailbox `address`, fetched every
    /// `interval_secs` seconds (clamped to at least 1).
    pub fn new(
        runtime: Arc<CompanyRuntime>,
        receiver: Arc<dyn MailReceiver>,
        creds: ImapCredentials,
        address: String,
        interval_secs: u64,
    ) -> Self {
        Self {
            runtime,
            receiver,
            creds,
            address,
            interval: Duration::from_secs(interval_secs.max(1)),
        }
    }

    /// Fetches new mail and files each message. Returns the count filed. Skips
    /// (returning `Ok(0)`) when the company is not running — scale-to-zero
    /// leaves unseen mail parked in the mailbox until the next tick after wake.
    pub async fn tick(&self) -> crate::Result<usize> {
        if self.runtime.ensure_running().await.is_err() {
            return Ok(0);
        }
        let messages = self.receiver.fetch_new(&self.creds).await?;
        let filed = messages.len();
        for m in messages {
            let record = EmailRecord {
                id: generate_id(),
                inbox: local_part(&self.address),
                from_name: m.from_name,
                from_email: m.from_email,
                subject: m.subject,
                body: m.body,
                at_millis: now_millis(),
                read: false,
                outbound: false,
            };
            file_and_notify(&self.runtime, &self.address, record).await?;
        }
        Ok(filed)
    }

    /// Spawns the interval loop; stops on `shutdown`. Mirrors
    /// [`CompanyScheduler::spawn`](crate::runtime::scheduler::CompanyScheduler::spawn).
    pub fn spawn(self, shutdown: Arc<Notify>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.notified() => break,
                    _ = tokio::time::sleep(self.interval) => {
                        if let Err(err) = self.tick().await {
                            tracing::warn!(company = %self.runtime.id(), %err, "mailbox poll failed");
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::company::CompanyManifest;
    use crate::runtime::RuntimeBuilder;
    use crate::server::ops::mailer::{InboundEmail, RecordingMailReceiver};

    fn tmp_home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("opencompany-mailpoll-{}", generate_id()))
    }

    fn manifest() -> CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "full"
        "#;
        toml::from_str(toml_src).expect("parse manifest")
    }

    #[tokio::test]
    async fn tick_files_and_notifies_each_message() {
        let home = tmp_home();
        let runtime = Arc::new(
            RuntimeBuilder::fs_defaults(home.clone(), manifest())
                .await
                .unwrap(),
        );

        let rx = Arc::new(RecordingMailReceiver::new());
        rx.push_batch(vec![
            InboundEmail {
                from_name: "A".into(),
                from_email: "a@x".into(),
                subject: "s1".into(),
                body: "b1".into(),
            },
            InboundEmail {
                from_name: "B".into(),
                from_email: "b@x".into(),
                subject: "s2".into(),
                body: "b2".into(),
            },
        ]);
        let creds = ImapCredentials {
            host: "h".into(),
            port: 993,
            username: "u".into(),
            password: "p".into(),
        };
        let poller = MailboxPoller::new(
            runtime.clone(),
            rx.clone(),
            creds,
            "acme@opencompany.work".into(),
            60,
        );
        let n = poller.tick().await.unwrap();
        assert_eq!(n, 2);

        // Both filed to the "acme" inbox (the local part of the address).
        assert_eq!(
            runtime
                .inbox()
                .messages(runtime.id(), "acme", 10, 0)
                .await
                .unwrap()
                .len(),
            2
        );
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn tick_skips_while_not_running() {
        let home = tmp_home();
        let runtime = Arc::new(
            RuntimeBuilder::fs_defaults(home.clone(), manifest())
                .await
                .unwrap(),
        );
        // Mark the company paused so `ensure_running` rejects. `build()` always
        // persists an initial record (lifecycle "running"), so this is a load
        // + mutate + save rather than a fresh insert. `store` is `pub(crate)`
        // on `CompanyRuntime`, reachable from here inside the crate.
        let mut record = runtime.store.load(runtime.id()).await.unwrap().unwrap();
        record.lifecycle = "paused".into();
        runtime.store.save(&record).await.unwrap();

        let rx = Arc::new(RecordingMailReceiver::new());
        rx.push_batch(vec![InboundEmail {
            from_name: "A".into(),
            from_email: "a@x".into(),
            subject: "s1".into(),
            body: "b1".into(),
        }]);
        let creds = ImapCredentials {
            host: "h".into(),
            port: 993,
            username: "u".into(),
            password: "p".into(),
        };
        let poller = MailboxPoller::new(
            runtime.clone(),
            rx.clone(),
            creds,
            "acme@opencompany.work".into(),
            60,
        );
        let n = poller.tick().await.unwrap();
        assert_eq!(n, 0);
        assert_eq!(rx.calls(), 0);
        tokio::fs::remove_dir_all(&home).await.ok();
    }
}
