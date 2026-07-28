//! IMAP inbound: credentials (always compiled) + the async-imap transport
//! (feature-gated in Task 4). Mirrors `smtp.rs`, where `SmtpCredentials` is
//! always compiled and only `LettreMailSender` is gated behind `smtp`.
use serde::{Deserialize, Serialize};

/// Credentials for polling one IMAP mailbox — **secret** (`password`).
#[derive(Clone, Serialize, Deserialize)]
pub struct ImapCredentials {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for ImapCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the password.
        f.debug_struct("ImapCredentials")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_password() {
        let creds = ImapCredentials {
            host: "imap.example.com".into(),
            port: 993,
            username: "acme@opencompany.work".into(),
            password: "SUPER-SECRET-PW-123".into(),
        };
        let rendered = format!("{creds:?}");
        assert!(
            !rendered.contains("SUPER-SECRET-PW-123"),
            "the password leaked into Debug: {rendered}"
        );
        assert!(rendered.contains("imap.example.com"));
        assert!(rendered.contains("993"));
        assert!(rendered.contains("acme@opencompany.work"));
    }
}

#[cfg(feature = "imap")]
use crate::error::OpenCompanyError;
#[cfg(feature = "imap")]
use crate::server::ops::mailer::{FetchedEmail, InboundEmail, MailReceiver};

/// Upper bound on one IMAP round trip (connect through logout, or a
/// mark-seen exchange). CodeRabbit: an unresponsive server must not hang the
/// poller tick forever.
#[cfg(feature = "imap")]
const IMAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Parse one RFC822 message into an `InboundEmail` (plain-text body, v1).
#[cfg(feature = "imap")]
pub(crate) fn parse_message(raw: &[u8]) -> InboundEmail {
    use mail_parser::MessageParser;

    let parsed = MessageParser::default().parse(raw);
    let (from_name, from_email) = parsed
        .as_ref()
        .and_then(|m| m.from())
        .and_then(|a| a.first())
        .map(|addr| {
            (
                addr.name().unwrap_or_default().to_string(),
                addr.address().unwrap_or_default().to_string(),
            )
        })
        .unwrap_or_default();
    let subject = parsed
        .as_ref()
        .and_then(|m| m.subject())
        .unwrap_or_default()
        .to_string();
    let body = parsed
        .as_ref()
        .and_then(|m| m.body_text(0))
        .map(|c| c.to_string())
        .unwrap_or_default();
    InboundEmail {
        from_name,
        from_email,
        subject,
        body,
    }
}

/// Real IMAP poller: connect over TLS, SELECT INBOX, SEARCH UNSEEN, FETCH,
/// parse, then mark the fetched messages `\Seen`.
///
/// `async-imap`'s `runtime-tokio` build links no TLS of its own — `Client::new`
/// just takes an already-connected stream — so this type drives the TLS
/// handshake itself with `tokio-rustls`, trusting the Mozilla root set baked
/// into `webpki-roots` (no OS trust store lookup, so the build stays fully
/// offline/reproducible).
#[cfg(feature = "imap")]
pub struct AsyncImapReceiver;

#[cfg(feature = "imap")]
type ImapSession = async_imap::Session<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

#[cfg(feature = "imap")]
impl AsyncImapReceiver {
    /// Builds a rustls TLS connector trusting the Mozilla root set.
    ///
    /// Installs the process-wide rustls `CryptoProvider` on first use. This is
    /// idempotent — `install_default` returning `Err` just means some other
    /// feature (e.g. `smtp`'s `lettre` stack) already installed one in this
    /// process, which is fine, we only need *a* provider installed.
    fn tls_connector() -> Result<tokio_rustls::TlsConnector, OpenCompanyError> {
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let root_store: tokio_rustls::rustls::RootCertStore =
            webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
        let config = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        Ok(tokio_rustls::TlsConnector::from(std::sync::Arc::new(
            config,
        )))
    }

    /// Connects, logs in, and SELECTs INBOX. Shared by `fetch_new` and
    /// `mark_seen` — each opens its own short-lived session (kept simple
    /// rather than a pooled/reused connection across the two calls).
    async fn connect(creds: &ImapCredentials) -> Result<ImapSession, OpenCompanyError> {
        let tcp = tokio::net::TcpStream::connect((creds.host.as_str(), creds.port))
            .await
            .map_err(|e| OpenCompanyError::Store(format!("imap connect: {e}")))?;
        let domain = tokio_rustls::rustls::pki_types::ServerName::try_from(creds.host.clone())
            .map_err(|e| OpenCompanyError::Store(format!("imap tls domain: {e}")))?;
        let tls_stream = Self::tls_connector()?
            .connect(domain, tcp)
            .await
            .map_err(|e| OpenCompanyError::Store(format!("imap tls connect: {e}")))?;

        let client = async_imap::Client::new(tls_stream);
        let mut session = client
            .login(&creds.username, &creds.password)
            .await
            .map_err(|(e, _)| OpenCompanyError::Store(format!("imap login: {e}")))?;

        session
            .select("INBOX")
            .await
            .map_err(|e| OpenCompanyError::Store(format!("imap select: {e}")))?;

        Ok(session)
    }

    /// The actual fetch, unwrapped by [`Self::fetch_new`]'s timeout: `UID
    /// SEARCH UNSEEN` (immune to sequence-number shift on expunge, unlike
    /// plain `SEARCH`) then `UID FETCH ... BODY.PEEK[]`. `BODY.PEEK[]` is the
    /// point — unlike `RFC822`/`BODY[]`, it does NOT set `\Seen`, so a message
    /// that fails to file durably stays unseen and is retried on the next
    /// tick instead of being silently dropped.
    async fn fetch_new_inner(
        creds: &ImapCredentials,
    ) -> Result<Vec<FetchedEmail>, OpenCompanyError> {
        use futures::TryStreamExt;

        let mut session = Self::connect(creds).await?;

        let unseen = session
            .uid_search("UNSEEN")
            .await
            .map_err(|e| OpenCompanyError::Store(format!("imap search: {e}")))?;

        let mut out = Vec::new();
        if !unseen.is_empty() {
            let mut uids: Vec<u32> = unseen.into_iter().collect();
            uids.sort_unstable();
            let set = uids
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut fetches = session
                .uid_fetch(&set, "UID BODY.PEEK[]")
                .await
                .map_err(|e| OpenCompanyError::Store(format!("imap fetch: {e}")))?;
            while let Some(f) = fetches
                .try_next()
                .await
                .map_err(|e| OpenCompanyError::Store(format!("imap fetch stream: {e}")))?
            {
                if let (Some(uid), Some(body)) = (f.uid, f.body()) {
                    out.push(FetchedEmail {
                        uid,
                        email: parse_message(body),
                    });
                }
            }
        }

        let _ = session.logout().await;
        Ok(out)
    }

    /// The actual mark-seen exchange, unwrapped by [`Self::mark_seen`]'s
    /// timeout: `UID STORE <uids> +FLAGS.SILENT (\Seen)`. Callers only pass
    /// UIDs of messages already durably filed — see [`MailReceiver::mark_seen`].
    async fn mark_seen_inner(
        creds: &ImapCredentials,
        uids: &[u32],
    ) -> Result<(), OpenCompanyError> {
        use futures::TryStreamExt;

        let mut session = Self::connect(creds).await?;

        let set = uids
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        {
            let updates = session
                .uid_store(&set, "+FLAGS.SILENT (\\Seen)")
                .await
                .map_err(|e| OpenCompanyError::Store(format!("imap store: {e}")))?;
            // `.SILENT` normally means the server sends no untagged FETCH
            // responses, but drain the stream to completion regardless so any
            // that do arrive don't desync the next command on this session.
            let _: Vec<_> = updates
                .try_collect()
                .await
                .map_err(|e| OpenCompanyError::Store(format!("imap store stream: {e}")))?;
        }

        let _ = session.logout().await;
        Ok(())
    }
}

#[cfg(feature = "imap")]
#[async_trait::async_trait]
impl MailReceiver for AsyncImapReceiver {
    async fn fetch_new(
        &self,
        creds: &ImapCredentials,
    ) -> Result<Vec<FetchedEmail>, OpenCompanyError> {
        match tokio::time::timeout(IMAP_TIMEOUT, Self::fetch_new_inner(creds)).await {
            Ok(result) => result,
            Err(_) => Err(OpenCompanyError::Store("imap: timed out".into())),
        }
    }

    async fn mark_seen(
        &self,
        creds: &ImapCredentials,
        uids: &[u32],
    ) -> Result<(), OpenCompanyError> {
        if uids.is_empty() {
            return Ok(());
        }
        match tokio::time::timeout(IMAP_TIMEOUT, Self::mark_seen_inner(creds, uids)).await {
            Ok(result) => result,
            Err(_) => Err(OpenCompanyError::Store("imap: timed out".into())),
        }
    }
}

#[cfg(all(test, feature = "imap"))]
mod imap_tests {
    use super::*;

    #[test]
    fn parse_message_extracts_headers_and_text_body() {
        let raw = b"From: Alice <alice@example.com>\r\nSubject: Hi\r\n\r\nHello world\r\n";
        let msg = parse_message(raw);
        assert_eq!(msg.from_email, "alice@example.com");
        assert_eq!(msg.from_name, "Alice");
        assert_eq!(msg.subject, "Hi");
        assert!(msg.body.contains("Hello world"));
    }
}
