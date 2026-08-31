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
    /// When set, the runtime to file into is looked up here every tick so a
    /// runtime swap (issue #290) reaches inbound mail. `None` keeps the boot
    /// snapshot.
    registry: Option<crate::runtime::CompanyRegistry>,
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
            registry: None,
            receiver,
            creds,
            address,
            interval: Duration::from_secs(interval_secs.max(1)),
        }
    }

    /// Issue #290: re-read `registry` for this company on every tick, instead of
    /// driving the `Arc<CompanyRuntime>` snapshotted at boot.
    ///
    /// Without this, a runtime swap never reaches this loop: it keeps driving a
    /// runtime that has been replaced and quiesced, so every tick fails. Opted
    /// into by the boot path, so existing callers keep the snapshot behaviour.
    pub fn following(mut self, registry: crate::runtime::CompanyRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// The runtime to drive this tick: whatever is registered now, else the
    /// snapshot taken at construction.
    fn runtime(&self) -> Arc<CompanyRuntime> {
        self.registry
            .as_ref()
            .and_then(|registry| registry.get(self.runtime.id()))
            .unwrap_or_else(|| self.runtime.clone())
    }

    /// Fetches new mail and files each message. Returns the count filed. Skips
    /// (returning `Ok(0)`) when the company is not running — scale-to-zero
    /// leaves unseen mail parked in the mailbox until the next tick after wake.
    ///
    /// Messages are fetched without being marked `\Seen` (see
    /// [`MailReceiver::fetch_new`]); only once a message is durably filed does
    /// its UID get queued for [`MailReceiver::mark_seen`], and that ack is sent
    /// after the whole batch has been attempted. A storage failure on one
    /// message is logged and skipped rather than aborting the batch — both
    /// choices mean one bad message can never mark itself (or any message
    /// after it) seen without having been filed, so nothing is silently lost.
    pub async fn tick(&self) -> crate::Result<usize> {
        let runtime = self.runtime();
        if runtime.ensure_running().await.is_err() {
            return Ok(0);
        }
        let messages = self.receiver.fetch_new(&self.creds).await?;
        let mut filed_uids = Vec::with_capacity(messages.len());
        for m in messages {
            let record = EmailRecord {
                id: generate_id(),
                inbox: local_part(&self.address),
                from_name: m.email.from_name,
                from_email: m.email.from_email,
                subject: m.email.subject,
                body: m.email.body,
                at_millis: now_millis(),
                read: false,
                outbound: false,
            };
            match file_and_notify(&runtime, &self.address, record).await {
                Ok(()) => filed_uids.push(m.uid),
                Err(err) => {
                    tracing::warn!(
                        company = %self.runtime.id(),
                        uid = m.uid,
                        %err,
                        "failed to file inbound email; leaving unseen for retry"
                    );
                }
            }
        }
        let filed = filed_uids.len();
        if !filed_uids.is_empty()
            && let Err(err) = self.receiver.mark_seen(&self.creds, &filed_uids).await
        {
            tracing::warn!(company = %self.runtime.id(), %err, "failed to mark filed emails seen");
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
    use crate::ports::inbox::InboxMeta;
    use crate::ports::types::CompanyId;
    use crate::ports::types::SecretValue;
    use crate::runtime::RuntimeBuilder;
    use crate::server::ops::mailer::{FetchedEmail, InboundEmail, RecordingMailReceiver};
    use crate::store::FsInboxStore;

    fn tmp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-mailpoll-")
            .tempdir()
            .expect("tempdir")
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
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let runtime = Arc::new(
            RuntimeBuilder::fs_defaults(home.clone(), manifest())
                .await
                .unwrap(),
        );

        let rx = Arc::new(RecordingMailReceiver::new());
        rx.push_batch(vec![
            FetchedEmail {
                uid: 10,
                email: InboundEmail {
                    from_name: "A".into(),
                    from_email: "a@x".into(),
                    subject: "s1".into(),
                    body: "b1".into(),
                },
            },
            FetchedEmail {
                uid: 11,
                email: InboundEmail {
                    from_name: "B".into(),
                    from_email: "b@x".into(),
                    subject: "s2".into(),
                    body: "b2".into(),
                },
            },
        ]);
        let creds = ImapCredentials {
            host: "h".into(),
            port: 993,
            username: "u".into(),
            password: SecretValue("p".into()),
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
        // Both durably filed, so both UIDs — and only those UIDs — are acked.
        assert_eq!(rx.marked(), vec![10, 11]);
    }

    /// A wrapper [`InboxStore`] delegating to a real `FsInboxStore`, except
    /// `append` fails on its Nth call (1-indexed) — simulates one message in a
    /// batch failing to file durably without touching `file_and_notify` itself.
    struct NthAppendFailsInbox {
        inner: FsInboxStore,
        fail_on_call: usize,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ports::inbox::InboxStore for NthAppendFailsInbox {
        async fn inboxes(&self, company: &CompanyId) -> crate::Result<Vec<InboxMeta>> {
            self.inner.inboxes(company).await
        }
        async fn set_enabled(
            &self,
            company: &CompanyId,
            key: &str,
            meta: &InboxMeta,
        ) -> crate::Result<()> {
            self.inner.set_enabled(company, key, meta).await
        }
        async fn messages(
            &self,
            company: &CompanyId,
            key: &str,
            limit: usize,
            offset: usize,
        ) -> crate::Result<Vec<EmailRecord>> {
            self.inner.messages(company, key, limit, offset).await
        }
        async fn append(&self, company: &CompanyId, msg: &EmailRecord) -> crate::Result<()> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n == self.fail_on_call {
                return Err(crate::error::OpenCompanyError::Store(
                    "simulated storage failure".into(),
                ));
            }
            self.inner.append(company, msg).await
        }
        async fn mark_read(
            &self,
            company: &CompanyId,
            key: &str,
            ids: Option<&[String]>,
        ) -> crate::Result<u64> {
            self.inner.mark_read(company, key, ids).await
        }
    }

    #[tokio::test]
    async fn tick_continues_past_one_filing_failure_and_only_marks_seen_the_filed_uid() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let inbox = Arc::new(NthAppendFailsInbox {
            inner: FsInboxStore::new(home.clone()),
            fail_on_call: 2,
            calls: Default::default(),
        });
        let runtime = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest())
                .with_inbox(inbox)
                .build()
                .await
                .unwrap(),
        );

        let rx = Arc::new(RecordingMailReceiver::new());
        rx.push_batch(vec![
            FetchedEmail {
                uid: 20,
                email: InboundEmail {
                    from_name: "A".into(),
                    from_email: "a@x".into(),
                    subject: "s1".into(),
                    body: "b1".into(),
                },
            },
            FetchedEmail {
                uid: 21,
                email: InboundEmail {
                    from_name: "B".into(),
                    from_email: "b@x".into(),
                    subject: "s2".into(),
                    body: "b2".into(),
                },
            },
            FetchedEmail {
                uid: 22,
                email: InboundEmail {
                    from_name: "C".into(),
                    from_email: "c@x".into(),
                    subject: "s3".into(),
                    body: "b3".into(),
                },
            },
        ]);
        let creds = ImapCredentials {
            host: "h".into(),
            port: 993,
            username: "u".into(),
            password: SecretValue("p".into()),
        };
        let poller = MailboxPoller::new(
            runtime.clone(),
            rx.clone(),
            creds,
            "acme@opencompany.work".into(),
            60,
        );

        // Message 2 (uid 21) fails to file; messages 1 and 3 must still both
        // be attempted and filed — one bad message neither aborts the batch
        // nor gets (nor blocks a later message from getting) marked seen.
        let n = poller.tick().await.unwrap();
        assert_eq!(n, 2, "messages 1 and 3 filed; message 2 did not");
        assert_eq!(
            runtime
                .inbox()
                .messages(runtime.id(), "acme", 10, 0)
                .await
                .unwrap()
                .len(),
            2
        );
        // Only the successfully-filed UIDs are acked — 21 must never appear,
        // or it would be lost forever (marked seen without ever being filed).
        assert_eq!(rx.marked(), vec![20, 22]);
    }

    #[tokio::test]
    async fn tick_skips_while_not_running() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
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
        rx.push_batch(vec![FetchedEmail {
            uid: 1,
            email: InboundEmail {
                from_name: "A".into(),
                from_email: "a@x".into(),
                subject: "s1".into(),
                body: "b1".into(),
            },
        }]);
        let creds = ImapCredentials {
            host: "h".into(),
            port: 993,
            username: "u".into(),
            password: SecretValue("p".into()),
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
    }
}
