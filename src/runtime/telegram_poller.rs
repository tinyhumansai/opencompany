//! Per-company Telegram `getUpdates` long-polling listener (issue #203).
//!
//! This is the inbound path that works **without a public URL**. Telegram's
//! webhook delivery ([`crate::server::hooks`]) requires a publicly reachable
//! https endpoint, so on localhost, behind NAT, or on any self-hosted box it
//! can never fire — the console used to hand the operator a
//! `http://127.0.0.1:8091/hooks/<company>/telegram` URL that Telegram's servers
//! cannot resolve, leaving inbound DMs dead. Mirroring OpenHuman, the host
//! instead dials **out** to `api.telegram.org` and holds a long poll open.
//!
//! Structured like [`MailboxPoller`](crate::runtime::mailbox_poller): an
//! injectable loop whose [`tick`](TelegramPoller::tick) is a plain async fn, so
//! every branch is testable offline against
//! [`RecordingTelegramApi`](crate::company::telegram::RecordingTelegramApi).
//!
//! ## What one tick does
//!
//! 1. Reads the bot token from the company's
//!    [`SecretStore`](crate::ports::SecretStore). Unset (or cleared) is idle,
//!    not an error — the operator may paste a token at any time and the very
//!    next tick picks it up, with no restart. A **changed** token resets the
//!    poll offset, since offsets are per-bot.
//! 2. Skips while the company is asleep (scale-to-zero). Unread updates stay
//!    parked on Telegram's side and arrive on the first tick after wake.
//! 3. Handshakes once per token: Telegram rejects `getUpdates` with `409
//!    Conflict` while a webhook is registered, so the poller asks
//!    `getWebhookInfo` first. See [`TelegramPoller::handshake`] for how the two
//!    modes divide.
//! 4. Long-polls, and runs each update as an ordinary company turn
//!    ([`CompanyEvent::WebhookReceived`] on the `telegram` channel) whose reply
//!    is addressed back to the origin chat — byte-identical routing to the
//!    webhook path, which is what keeps the two inbound paths in agreement.
//!
//! ## Delivery semantics
//!
//! The poll offset is **in-memory only, by design**. Telegram itself holds the
//! confirmed watermark: asking for `max(update_id) + 1` is what acks a batch,
//! and a fresh process asking with no offset gets only what was never
//! confirmed. So a restart resumes exactly where it left off without any local
//! persistence — and cannot replay an already-answered DM. The offset advances
//! only after a batch has been processed, so a crash mid-batch redelivers that
//! batch (at-least-once) rather than dropping it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::company::runtime::CompanyRuntime;
use crate::company::telegram::{
    TELEGRAM_CHANNEL, TELEGRAM_TOKEN_KEY, TelegramApi, deliver_replies, scrub_token, update_id,
};
use crate::ports::types::CompanyEvent;

/// Default long-poll hold, in seconds. Also the back-off between idle re-checks
/// (no token stored, company asleep, or a webhook owning inbound).
pub const DEFAULT_POLL_SECONDS: u64 = 30;

/// What one [`TelegramPoller::tick`] did. Drives the loop's pacing: a poll that
/// actually ran was already paced by Telegram's server-side hold, so the loop
/// goes straight round again; anything else backs off for the idle interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollOutcome {
    /// Nothing to poll: no bot token stored, the company is not running, or a
    /// webhook owns inbound on this host.
    Idle,
    /// A `getUpdates` long poll ran and produced this many company turns.
    Polled(usize),
}

/// Drives one company's Telegram inbound over `getUpdates` long-polling.
pub struct TelegramPoller {
    runtime: Arc<CompanyRuntime>,
    api: Arc<dyn TelegramApi>,
    /// Long-poll hold handed to `getUpdates`, and the idle back-off.
    poll: Duration,
    /// Whether this host could serve a Telegram webhook at all — i.e. whether a
    /// publicly reachable `https` base URL is configured. Decides what a
    /// pre-existing webhook registration means (see [`Self::handshake`]).
    webhook_capable: bool,
    /// The token the current offset belongs to; offsets are per-bot.
    token: Option<String>,
    /// The next `update_id` to ask for — `None` means "whatever Telegram still
    /// holds unconfirmed", which is the correct cold-start ask.
    offset: Option<i64>,
    /// Whether the webhook handshake has settled for the current token. Cleared
    /// on a token change and after any poll error, so a webhook registered
    /// mid-flight is noticed rather than 409-looping forever.
    handshaked: bool,
}

impl TelegramPoller {
    /// Binds a poller to `runtime`, long-polling for `poll_secs` at a time
    /// (clamped to at least 1 so the loop can never spin).
    ///
    /// `webhook_capable` is the host's own answer to "could Telegram reach my
    /// `/hooks/...` route?" — see
    /// [`AppConfig::public_webhook_base_url`](crate::app::AppConfig::public_webhook_base_url).
    pub fn new(
        runtime: Arc<CompanyRuntime>,
        api: Arc<dyn TelegramApi>,
        poll_secs: u64,
        webhook_capable: bool,
    ) -> Self {
        Self {
            runtime,
            api,
            poll: Duration::from_secs(poll_secs.max(1)),
            webhook_capable,
            token: None,
            offset: None,
            handshaked: false,
        }
    }

    /// The bot token for this company, or `None` when unset/blank. An empty
    /// stored value means "cleared" everywhere else in the Telegram surface, so
    /// it means the same here.
    async fn load_token(&self) -> crate::Result<Option<String>> {
        Ok(self
            .runtime
            .secrets()
            .get(self.runtime.id(), TELEGRAM_TOKEN_KEY)
            .await?
            .map(|v| v.expose().to_string())
            .filter(|v| !v.is_empty()))
    }

    /// Settles which inbound path owns this bot, returning whether to poll.
    ///
    /// A registered webhook is the only thing that can block `getUpdates`, and
    /// what it means depends entirely on whether this host can actually be
    /// reached:
    ///
    /// - **Webhook-capable host** (a public https URL is configured): the
    ///   operator opted into the hosted fast-path, and it works. The poller
    ///   stands by so the two paths never both consume the same update, and
    ///   re-checks each idle tick, so deleting the webhook resumes polling.
    /// - **Not webhook-capable** — this is issue #203 exactly: a webhook is
    ///   registered against a URL Telegram can never deliver to, so inbound is
    ///   silently dead and `getUpdates` is refused as well. The poller reclaims
    ///   inbound by clearing the stale registration, then polls. Nothing is
    ///   lost: `deleteWebhook` keeps pending updates, so anything that queued
    ///   up arrives in the first batch.
    async fn handshake(&mut self, token: &str) -> crate::Result<bool> {
        match self.api.get_webhook_info(token).await? {
            Some(url) if !url.trim().is_empty() => {
                if self.webhook_capable {
                    tracing::debug!(
                        company = %self.runtime.id(),
                        "telegram webhook is registered and this host is publicly reachable; \
                         polling stands by"
                    );
                    return Ok(false);
                }
                tracing::warn!(
                    company = %self.runtime.id(),
                    "telegram webhook is registered but this host has no public https URL, so \
                     Telegram cannot deliver to it; clearing it and switching to getUpdates polling"
                );
                self.api.delete_webhook(token).await?;
            }
            _ => {}
        }
        self.handshaked = true;
        Ok(true)
    }

    /// Polls once and runs a company turn per actionable update.
    ///
    /// Returns [`PollOutcome::Idle`] when there was nothing to poll (see the
    /// module docs for the three cases). A turn that fails is logged and
    /// skipped — the batch still advances, because Telegram would otherwise
    /// redeliver the same poisoned update forever.
    pub async fn tick(&mut self) -> crate::Result<PollOutcome> {
        let Some(token) = self.load_token().await? else {
            // Cleared credentials: forget the offset so a later token starts
            // clean rather than acking another bot's update ids.
            self.token = None;
            self.offset = None;
            self.handshaked = false;
            return Ok(PollOutcome::Idle);
        };
        if self.token.as_deref() != Some(token.as_str()) {
            self.token = Some(token.clone());
            self.offset = None;
            self.handshaked = false;
        }
        if self.runtime.ensure_running().await.is_err() {
            return Ok(PollOutcome::Idle);
        }
        if !self.handshaked && !self.handshake(&token).await? {
            return Ok(PollOutcome::Idle);
        }

        let updates = match self
            .api
            .get_updates(&token, self.offset, self.poll.as_secs())
            .await
        {
            Ok(updates) => updates,
            Err(err) => {
                // Re-handshake next tick: the most likely cause of a hard
                // failure here is a webhook registered while we were polling
                // (Telegram answers 409), and that is exactly what the
                // handshake resolves.
                self.handshaked = false;
                return Err(crate::error::OpenCompanyError::Store(scrub_token(
                    &err.to_string(),
                    &token,
                )));
            }
        };

        let mut turns = 0usize;
        for update in updates {
            // Ack before running: `update_id` is the only thing that stops
            // Telegram redelivering, and a turn that panics the batch must not
            // strand the poller on the same update.
            if let Some(id) = update_id(&update) {
                self.offset = Some(match self.offset {
                    Some(current) => current.max(id + 1),
                    None => id + 1,
                });
            }
            let event = CompanyEvent::WebhookReceived {
                channel: TELEGRAM_CHANNEL.to_string(),
                body: update,
            };
            match self.runtime.run_cycle(vec![event]).await {
                Ok(report) => {
                    turns += 1;
                    deliver_replies(
                        self.api.as_ref(),
                        self.runtime.id(),
                        &token,
                        &report.responses,
                    )
                    .await;
                }
                Err(err) => {
                    tracing::warn!(
                        company = %self.runtime.id(),
                        "telegram polled cycle failed: {}",
                        scrub_token(&err.to_string(), &token)
                    );
                }
            }
        }
        Ok(PollOutcome::Polled(turns))
    }

    /// Spawns the poll loop; stops on `shutdown`.
    ///
    /// Unlike the interval-driven [`MailboxPoller`](crate::runtime::mailbox_poller),
    /// pacing comes from Telegram's server-side long-poll hold: a poll that ran
    /// loops straight round, and only an idle or failed tick backs off. Being
    /// dropped mid-tick is safe — an unacked batch is simply redelivered.
    pub fn spawn(mut self, shutdown: Arc<Notify>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.notified() => break,
                    backoff = async {
                        match self.tick().await {
                            Ok(PollOutcome::Polled(_)) => false,
                            Ok(PollOutcome::Idle) => true,
                            Err(err) => {
                                tracing::warn!(
                                    company = %self.runtime.id(),
                                    %err,
                                    "telegram poll failed"
                                );
                                true
                            }
                        }
                    } => {
                        if backoff {
                            tokio::select! {
                                _ = shutdown.notified() => break,
                                _ = tokio::time::sleep(self.poll) => {}
                            }
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
    use crate::company::telegram::RecordingTelegramApi;
    use crate::ports::types::SecretValue;
    use crate::runtime::RuntimeBuilder;

    const TOKEN: &str = "7654321:AAExampleBotTokenNeverLeaks";

    fn tmp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-tgpoll-")
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

    fn update(id: i64, chat_id: i64, text: &str) -> serde_json::Value {
        serde_json::json!({
            "update_id": id,
            "message": {
                "message_id": id,
                "from": { "id": 999, "username": "bob" },
                "chat": { "id": chat_id, "type": "private" },
                "text": text,
            }
        })
    }

    async fn company(home: &std::path::Path) -> Arc<CompanyRuntime> {
        Arc::new(
            RuntimeBuilder::fs_defaults(home.to_path_buf(), manifest())
                .await
                .expect("build runtime"),
        )
    }

    async fn store_token(runtime: &CompanyRuntime, token: &str) {
        runtime
            .secrets()
            .set(
                runtime.id(),
                TELEGRAM_TOKEN_KEY,
                SecretValue(token.to_string()),
            )
            .await
            .expect("store token");
    }

    /// The whole point of the issue: a bot token alone — no webhook secret, no
    /// public URL, no `setWebhook` — is enough to receive a DM and answer it.
    #[tokio::test]
    async fn polls_updates_and_replies_without_any_webhook() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let runtime = company(&home).await;
        store_token(&runtime, TOKEN).await;

        let api = Arc::new(RecordingTelegramApi::new());
        api.push_updates(vec![update(41, 555, "status?")]);
        let mut poller = TelegramPoller::new(runtime.clone(), api.clone(), 1, false);

        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Polled(1));
        // The echo brain answers the DM back into the origin chat.
        let sent = api.sent();
        assert_eq!(sent.len(), 1, "one reply delivered: {sent:?}");
        assert_eq!(sent[0].0, 555);
        assert!(sent[0].1.contains("status?"), "reply echoes: {sent:?}");
        // Cold start asks with no offset; the next poll acks update 41.
        assert_eq!(api.poll_offsets(), vec![None]);
        assert_eq!(poller.offset, Some(42));

        // A second tick carries the ack forward even with nothing to fetch.
        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Polled(0));
        assert_eq!(api.poll_offsets(), vec![None, Some(42)]);
    }

    /// Issue #203's broken state: a webhook is registered against a URL Telegram
    /// can never reach, so inbound is dead *and* `getUpdates` is refused. The
    /// poller must clear it and take over rather than 409-loop.
    #[tokio::test]
    async fn clears_an_unreachable_webhook_and_takes_over_inbound() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let runtime = company(&home).await;
        store_token(&runtime, TOKEN).await;

        let api = Arc::new(RecordingTelegramApi::new());
        api.set_registered_webhook("http://127.0.0.1:8091/hooks/acme/telegram");
        api.push_updates(vec![update(7, 42, "hello")]);
        // `webhook_capable: false` — no public https URL on this host.
        let mut poller = TelegramPoller::new(runtime.clone(), api.clone(), 1, false);

        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Polled(1));
        assert_eq!(api.delete_webhook_calls(), 1, "stale webhook was cleared");
        assert_eq!(api.sent().len(), 1, "the parked DM got answered");
    }

    /// On a host Telegram *can* reach, a registered webhook is a deliberate
    /// choice. The poller must stand by rather than delete it or double-consume
    /// updates — and must re-check, so unregistering resumes polling.
    #[tokio::test]
    async fn stands_by_for_a_webhook_on_a_publicly_reachable_host() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let runtime = company(&home).await;
        store_token(&runtime, TOKEN).await;

        let api = Arc::new(RecordingTelegramApi::new());
        api.set_registered_webhook("https://acme.example/hooks/acme/telegram");
        api.push_updates(vec![update(1, 42, "hi")]);
        let mut poller = TelegramPoller::new(runtime.clone(), api.clone(), 1, true);

        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Idle);
        assert_eq!(api.delete_webhook_calls(), 0, "never clears a live webhook");
        assert!(api.poll_offsets().is_empty(), "never polled");
        assert!(api.sent().is_empty(), "the webhook route owns this update");

        // Standby is re-checked every tick, so removing the webhook resumes.
        api.delete_webhook(TOKEN).await.unwrap();
        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Polled(1));
        assert_eq!(api.sent().len(), 1);
    }

    /// No token stored is idle, not an error — and a token pasted later is
    /// picked up on the very next tick, with no restart.
    #[tokio::test]
    async fn idles_without_a_token_then_picks_one_up_live() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let runtime = company(&home).await;

        let api = Arc::new(RecordingTelegramApi::new());
        api.push_updates(vec![update(3, 9, "yo")]);
        let mut poller = TelegramPoller::new(runtime.clone(), api.clone(), 1, false);

        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Idle);
        assert!(api.poll_offsets().is_empty(), "no token means no call");

        store_token(&runtime, TOKEN).await;
        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Polled(1));

        // Clearing the credential parks the poller and drops the offset, so a
        // different bot's update ids can never be acked against it.
        store_token(&runtime, "").await;
        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Idle);
        assert_eq!(poller.offset, None);
    }

    /// Scale-to-zero: a paused company polls nothing. Updates wait on
    /// Telegram's side and arrive on the first tick after wake.
    #[tokio::test]
    async fn skips_while_the_company_is_not_running() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let runtime = company(&home).await;
        store_token(&runtime, TOKEN).await;
        let mut record = runtime.store.load(runtime.id()).await.unwrap().unwrap();
        record.lifecycle = "paused".into();
        runtime.store.save(&record).await.unwrap();

        let api = Arc::new(RecordingTelegramApi::new());
        api.push_updates(vec![update(1, 42, "hi")]);
        let mut poller = TelegramPoller::new(runtime.clone(), api.clone(), 1, false);

        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Idle);
        assert!(api.poll_offsets().is_empty());
    }

    /// A poll failure must never echo the bot token, and must re-arm the
    /// handshake so a mid-flight webhook registration is noticed.
    #[tokio::test]
    async fn poll_errors_are_scrubbed_and_rearm_the_handshake() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let runtime = company(&home).await;
        store_token(&runtime, TOKEN).await;

        let api = Arc::new(RecordingTelegramApi::new());
        let mut poller = TelegramPoller::new(runtime.clone(), api.clone(), 1, true);
        // Settle the handshake with no webhook registered...
        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Polled(0));
        assert!(poller.handshaked);

        // ...then register one, so the next poll conflicts.
        api.set_registered_webhook("https://acme.example/hooks/acme/telegram");
        let err = poller.tick().await.unwrap_err().to_string();
        assert!(!err.contains(TOKEN), "token leaked into the error: {err}");
        assert!(!poller.handshaked, "handshake re-armed after a failure");

        // Re-handshaking resolves it: the webhook owns inbound, so stand by.
        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Idle);
    }

    /// Changing the bot token discards the previous bot's offset — update ids
    /// are per-bot, so carrying one over would ack the wrong updates.
    #[tokio::test]
    async fn a_new_token_resets_the_offset() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let runtime = company(&home).await;
        store_token(&runtime, TOKEN).await;

        let api = Arc::new(RecordingTelegramApi::new());
        api.push_updates(vec![update(500, 1, "first bot")]);
        let mut poller = TelegramPoller::new(runtime.clone(), api.clone(), 1, false);
        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Polled(1));
        assert_eq!(poller.offset, Some(501));

        store_token(&runtime, "111111:AADifferentBotEntirely").await;
        assert_eq!(poller.tick().await.unwrap(), PollOutcome::Polled(0));
        assert_eq!(
            api.poll_offsets(),
            vec![None, None],
            "the second bot is asked cold, not at the first bot's offset"
        );
    }
}
