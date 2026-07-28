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

#[cfg(feature = "imap")]
use crate::error::OpenCompanyError;
#[cfg(feature = "imap")]
use crate::server::ops::mailer::{InboundEmail, MailReceiver};

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
}

#[cfg(feature = "imap")]
#[async_trait::async_trait]
impl MailReceiver for AsyncImapReceiver {
    async fn fetch_new(
        &self,
        creds: &ImapCredentials,
    ) -> Result<Vec<InboundEmail>, OpenCompanyError> {
        use futures::TryStreamExt;

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

        let unseen = session
            .search("UNSEEN")
            .await
            .map_err(|e| OpenCompanyError::Store(format!("imap search: {e}")))?;

        let mut out = Vec::new();
        if !unseen.is_empty() {
            let set = unseen
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",");
            // RFC822 fetch marks `\Seen` server-side; that IS the intended
            // dedup — do not add a NOOP/`BODY.PEEK` that would avoid it.
            let mut fetches = session
                .fetch(&set, "RFC822")
                .await
                .map_err(|e| OpenCompanyError::Store(format!("imap fetch: {e}")))?;
            while let Some(f) = fetches
                .try_next()
                .await
                .map_err(|e| OpenCompanyError::Store(format!("imap fetch stream: {e}")))?
            {
                if let Some(body) = f.body() {
                    out.push(parse_message(body));
                }
            }
        }

        let _ = session.logout().await;
        Ok(out)
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
