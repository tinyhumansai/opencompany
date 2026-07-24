//! The Telegram channel adapter: inbound-update parsing, outbound delivery, and
//! the token-scrubbing that keeps the bot credential out of every log and error.
//!
//! A company receives Telegram DMs through the signed webhook route
//! ([`crate::server::hooks`]) and replies back into the same chat. Inbound
//! routing mirrors OpenHuman: a bot DM is a **web/chat turn** tagged with its
//! origin (the Telegram `chat.id`), not a bespoke "telegram provider" — so the
//! brain runs an ordinary company turn and the reply is addressed back with an
//! [`OutboundMessage::reply_to`](crate::ports::types::OutboundMessage).
//!
//! ## Credentials are write-only
//!
//! The bot token and the webhook secret live in
//! [`SecretStore`](crate::ports::SecretStore) under [`TELEGRAM_TOKEN_KEY`] and
//! [`TELEGRAM_SECRET_KEY`] as **raw strings** — there is deliberately no
//! serializable credential struct, so neither value can be reflected into a
//! status/GET/error body by construction. The only thing a response ever
//! carries is the non-secret [`TelegramChannelStatus`](crate::server::ops::channels::TelegramChannelStatus).
//! [`scrub_token`] strips the token from any Telegram-API error text before it
//! reaches `tracing`, `report_error`, or an agent-visible surface.
//!
//! ## Network seam
//!
//! [`TelegramApi`] is the outbound seam. The default build and every test use
//! the in-memory [`RecordingTelegramApi`]; the real HTTPS `sendMessage` /
//! `setWebhook` transport ([`HttpTelegramApi`]) is gated behind the `telegram`
//! feature so the offline build links no HTTP client.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::OpenCompanyError;

/// The channel id inbound Telegram turns and their replies are tagged with.
pub const TELEGRAM_CHANNEL: &str = "telegram";

/// SecretStore key holding the raw bot token (write-only).
pub const TELEGRAM_TOKEN_KEY: &str = "telegram/token";

/// SecretStore key holding the raw webhook secret the inbound
/// `X-Telegram-Bot-Api-Secret-Token` header is verified against (write-only).
pub const TELEGRAM_SECRET_KEY: &str = "telegram/secret";

/// The header Telegram sends on every webhook delivery, carrying the secret
/// token configured via `setWebhook`. Verified before any update is parsed.
pub const SECRET_TOKEN_HEADER: &str = "x-telegram-bot-api-secret-token";

/// A normalized inbound Telegram message — the origin `chat.id`, the text, and a
/// best-effort human label for the sender. Produced from a raw update by
/// [`parse_inbound`]; non-message updates (edits without text, channel posts,
/// callback queries) yield `None` and are acknowledged without a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelegramInbound {
    /// The chat the message arrived in; the reply is delivered back here.
    pub chat_id: i64,
    /// The message text.
    pub text: String,
    /// A human label for the sender (`@username`, else a first name, else the
    /// numeric user id) — used only to attribute the turn, never for auth.
    pub from: String,
}

/// Parses a raw Telegram `Update` into a [`TelegramInbound`], or `None` when the
/// update carries no actionable text message.
///
/// Accepts both `message` and `edited_message`; anything without a `chat.id` and
/// a non-empty `text` (a photo, a sticker, a channel post) is ignored so the
/// webhook still answers `200` without running an empty turn.
pub fn parse_inbound(update: &serde_json::Value) -> Option<TelegramInbound> {
    let message = update
        .get("message")
        .or_else(|| update.get("edited_message"))?;

    let chat_id = message.get("chat")?.get("id")?.as_i64()?;
    let text = message.get("text")?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }

    let from = message
        .get("from")
        .map(|f| {
            if let Some(username) = f.get("username").and_then(|u| u.as_str()) {
                format!("@{username}")
            } else if let Some(first) = f.get("first_name").and_then(|n| n.as_str()) {
                first.to_string()
            } else if let Some(id) = f.get("id").and_then(|i| i.as_i64()) {
                id.to_string()
            } else {
                "telegram-user".to_string()
            }
        })
        .unwrap_or_else(|| "telegram-user".to_string());

    Some(TelegramInbound {
        chat_id,
        text: text.to_string(),
        from,
    })
}

/// Removes every occurrence of `token` from `text`, replacing it with `***`.
///
/// The bot token rides in the Telegram API URL (`…/bot<token>/sendMessage`), so
/// a transport error can echo it back verbatim. This is the single choke point
/// that keeps it out of logs, `report_error`, and any agent-visible message. An
/// empty token is a no-op (nothing to scrub).
pub fn scrub_token(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }
    text.replace(token, "***")
}

/// The outbound Telegram seam: send a reply and (re)register the webhook.
///
/// Mockable so every calling path is exercised offline; the real HTTPS
/// transport is feature-gated. An implementation MUST NOT surface the `token`
/// in a returned error unscrubbed — callers additionally run [`scrub_token`],
/// but the contract is defense in depth.
#[async_trait]
pub trait TelegramApi: Send + Sync {
    /// Delivers `text` to `chat_id` using bot `token` (Telegram `sendMessage`).
    async fn send_message(
        &self,
        token: &str,
        chat_id: i64,
        text: &str,
    ) -> Result<(), OpenCompanyError>;

    /// Points Telegram's webhook for bot `token` at `url`, guarded by `secret`
    /// (Telegram `setWebhook`, `secret_token` field).
    async fn set_webhook(
        &self,
        token: &str,
        url: &str,
        secret: &str,
    ) -> Result<(), OpenCompanyError>;
}

/// An offline [`TelegramApi`] that records deliveries for assertions and never
/// touches the network. It intentionally records only `(chat_id, text)` and the
/// webhook `url` — never the token — so a test proving "delivery carries no
/// credential" reads the recording directly.
#[derive(Clone, Default)]
pub struct RecordingTelegramApi {
    sent: Arc<std::sync::Mutex<Vec<(i64, String)>>>,
    webhooks: Arc<std::sync::Mutex<Vec<String>>>,
    /// When set, `send_message` fails and — modelling a real transport leaking
    /// the credential into its error URL — echoes the received token into the
    /// error text, so a scrubbing test has something to scrub.
    fail_echoing_token: bool,
}

impl RecordingTelegramApi {
    /// A recording API that always accepts.
    pub fn new() -> Self {
        Self::default()
    }

    /// A recording API whose `send_message` fails with an error that embeds the
    /// bot token — used to prove the caller scrubs it.
    pub fn failing_with_token_echo() -> Self {
        Self {
            fail_echoing_token: true,
            ..Self::default()
        }
    }

    /// Every `(chat_id, text)` delivered so far.
    pub fn sent(&self) -> Vec<(i64, String)> {
        self.sent
            .lock()
            .expect("telegram recorder poisoned")
            .clone()
    }

    /// Every webhook URL registered so far.
    pub fn webhooks(&self) -> Vec<String> {
        self.webhooks
            .lock()
            .expect("telegram recorder poisoned")
            .clone()
    }
}

#[async_trait]
impl TelegramApi for RecordingTelegramApi {
    async fn send_message(
        &self,
        token: &str,
        chat_id: i64,
        text: &str,
    ) -> Result<(), OpenCompanyError> {
        if self.fail_echoing_token {
            return Err(OpenCompanyError::Store(format!(
                "telegram sendMessage failed: https://api.telegram.org/bot{token}/sendMessage 401"
            )));
        }
        self.sent
            .lock()
            .expect("telegram recorder poisoned")
            .push((chat_id, text.to_string()));
        Ok(())
    }

    async fn set_webhook(
        &self,
        _token: &str,
        url: &str,
        _secret: &str,
    ) -> Result<(), OpenCompanyError> {
        self.webhooks
            .lock()
            .expect("telegram recorder poisoned")
            .push(url.to_string());
        Ok(())
    }
}

impl std::fmt::Debug for RecordingTelegramApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingTelegramApi")
            .field("sent", &self.sent().len())
            .field("webhooks", &self.webhooks().len())
            .finish()
    }
}

/// The real HTTPS transport to `api.telegram.org`. Gated behind the `telegram`
/// feature so the default build links no HTTP client. `Debug` never prints a
/// URL (the token rides in the path), and every error is scrubbed at the seam.
#[cfg(feature = "telegram")]
pub struct HttpTelegramApi {
    client: reqwest::Client,
}

#[cfg(feature = "telegram")]
impl HttpTelegramApi {
    /// Builds a transport over a fresh HTTPS client.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[cfg(feature = "telegram")]
impl Default for HttpTelegramApi {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "telegram")]
#[async_trait]
impl TelegramApi for HttpTelegramApi {
    async fn send_message(
        &self,
        token: &str,
        chat_id: i64,
        text: &str,
    ) -> Result<(), OpenCompanyError> {
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "chat_id": chat_id, "text": text }))
            .send()
            .await
            .map_err(|e| {
                OpenCompanyError::Store(scrub_token(
                    &format!("telegram sendMessage failed: {e}"),
                    token,
                ))
            })?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(OpenCompanyError::Store(scrub_token(
                &format!("telegram sendMessage returned {status}: {body}"),
                token,
            )))
        }
    }

    async fn set_webhook(
        &self,
        token: &str,
        url: &str,
        secret: &str,
    ) -> Result<(), OpenCompanyError> {
        let api = format!("https://api.telegram.org/bot{token}/setWebhook");
        let resp = self
            .client
            .post(&api)
            .json(&serde_json::json!({ "url": url, "secret_token": secret }))
            .send()
            .await
            .map_err(|e| {
                OpenCompanyError::Store(scrub_token(
                    &format!("telegram setWebhook failed: {e}"),
                    token,
                ))
            })?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(OpenCompanyError::Store(scrub_token(
                &format!("telegram setWebhook returned {status}: {body}"),
                token,
            )))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parses_a_plain_text_message() {
        let update = serde_json::json!({
            "update_id": 1,
            "message": {
                "message_id": 9,
                "from": { "id": 111, "username": "alice", "first_name": "Alice" },
                "chat": { "id": 222, "type": "private" },
                "text": "  hello there  "
            }
        });
        let inbound = parse_inbound(&update).expect("parses");
        assert_eq!(inbound.chat_id, 222);
        assert_eq!(inbound.text, "hello there");
        assert_eq!(inbound.from, "@alice");
    }

    #[test]
    fn falls_back_to_first_name_then_id_for_sender() {
        let by_first = serde_json::json!({
            "message": { "chat": { "id": 1 }, "from": { "id": 5, "first_name": "Bob" }, "text": "hi" }
        });
        assert_eq!(parse_inbound(&by_first).unwrap().from, "Bob");
        let by_id = serde_json::json!({
            "message": { "chat": { "id": 1 }, "from": { "id": 5 }, "text": "hi" }
        });
        assert_eq!(parse_inbound(&by_id).unwrap().from, "5");
    }

    #[test]
    fn accepts_edited_message_and_ignores_non_text_updates() {
        let edited = serde_json::json!({
            "edited_message": { "chat": { "id": 3 }, "text": "fixed" }
        });
        assert_eq!(parse_inbound(&edited).unwrap().text, "fixed");
        // A sticker/photo (no text) and a callback query are not turns.
        assert!(
            parse_inbound(&serde_json::json!({ "message": { "chat": { "id": 3 } } })).is_none()
        );
        assert!(parse_inbound(&serde_json::json!({ "callback_query": { "id": "x" } })).is_none());
        // Empty/whitespace text is not a turn either.
        assert!(
            parse_inbound(&serde_json::json!({ "message": { "chat": { "id": 3 }, "text": "  " } }))
                .is_none()
        );
    }

    #[test]
    fn scrub_removes_the_token_everywhere_it_appears() {
        let token = "123456:AAbb_ccDD";
        let leaked =
            format!("POST https://api.telegram.org/bot{token}/sendMessage -> 401 ({token})");
        let scrubbed = scrub_token(&leaked, token);
        assert!(
            !scrubbed.contains(token),
            "token survived scrub: {scrubbed}"
        );
        assert!(scrubbed.contains("***"));
        // Empty token is a no-op.
        assert_eq!(scrub_token("nothing to do", ""), "nothing to do");
    }

    #[tokio::test]
    async fn recording_api_captures_chat_and_text_but_not_token() {
        let api = RecordingTelegramApi::new();
        api.send_message("secret-token", 42, "on it").await.unwrap();
        api.set_webhook(
            "secret-token",
            "https://host/hooks/acme/telegram",
            "wh-secret",
        )
        .await
        .unwrap();
        assert_eq!(api.sent(), vec![(42, "on it".to_string())]);
        assert_eq!(
            api.webhooks(),
            vec!["https://host/hooks/acme/telegram".to_string()]
        );
        // The recording never retained the bot token.
        let dump = format!("{:?} {:?}", api.sent(), api.webhooks());
        assert!(!dump.contains("secret-token"));
    }

    #[tokio::test]
    async fn failing_api_echoes_token_so_callers_must_scrub() {
        let api = RecordingTelegramApi::failing_with_token_echo();
        let token = "999:zzZZ";
        let err = api.send_message(token, 1, "x").await.unwrap_err();
        // The raw error DOES carry the token — the caller is what scrubs it.
        assert!(err.to_string().contains(token));
        assert!(!scrub_token(&err.to_string(), token).contains(token));
    }
}
