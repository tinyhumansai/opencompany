//! IMAP inbound: credentials (always compiled) + the async-imap transport
//! (feature-gated in Task 4). Mirrors `smtp.rs`, where `SmtpCredentials` is
//! always compiled and only `LettreMailSender` is gated behind `smtp`.
use serde::{Deserialize, Serialize};

use crate::ports::types::SecretValue;

/// Credentials for polling one IMAP mailbox — **secret** (`password`).
///
/// The password is a [`SecretValue`] rather than a `String`, so *both*
/// rendering surfaces are guarded by the type instead of by an impl on this
/// struct. Before issue #1770 the `Debug` half was hand-written and tested
/// while the derived `Serialize` emitted the plaintext — the same blind spot
/// #1741 found on `SecretValue` itself, and the reason the fix belongs on the
/// field's type: a `#[derive(Debug, Serialize)]` here is now safe, and so is
/// the next struct that embeds one of these.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImapCredentials {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: SecretValue,
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    /// Issue #1770. The `Debug` half of this struct was hand-written *and*
    /// tested; the derived `Serialize` over a plain `String` password had no
    /// guard and no test, so `serde_json::to_value` over these credentials —
    /// or over anything embedding them — emitted the plaintext.
    ///
    /// The assertions go through containers `ImapCredentials` knows nothing
    /// about, because the guard now lives on the field's *type*: a struct with
    /// a plain `#[derive(Serialize)]`, plus `Option`, `Vec`, a map value and
    /// `#[serde(flatten)]` (a genuinely different serde code path), across
    /// both `to_string` and `to_value` (also different code paths in
    /// `serde_json`), and both `{:?}` and `{:#?}`.
    #[test]
    fn planted_password_never_reaches_debug_or_serialize() {
        use std::collections::BTreeMap;

        // Obviously fake, and distinctive enough that a substring hit is a
        // real hit. Same sentinel as the other planted-secret tests.
        const FAKE_SECRET: &str = "NOT-A-REAL-KEY-planted-for-tests";

        // Case-**insensitive**: a leak that arrives case-mangled is still a
        // leak, and an exact-case search reads it as clean.
        fn leaks(rendering: &str) -> bool {
            rendering
                .to_ascii_lowercase()
                .contains(&FAKE_SECRET.to_ascii_lowercase())
        }

        // Sanity: the detector detects. Without this every assertion below
        // could be vacuous and still read as green.
        assert!(
            leaks(&format!("password={}", FAKE_SECRET.to_ascii_lowercase())),
            "the leak detector cannot see a lowercased sentinel; every \
             assertion below would be vacuous"
        );

        /// The next struct somebody writes: derives `Serialize` and `Debug`
        /// with no idea a credential is in there.
        #[derive(Debug, Serialize)]
        struct UnsuspectingConfig {
            label: String,
            primary: ImapCredentials,
            optional: Option<ImapCredentials>,
            many: Vec<ImapCredentials>,
            by_name: BTreeMap<String, ImapCredentials>,
            #[serde(flatten)]
            nested: Nested,
        }

        /// Flattened, so serde uses `FlatMapSerializer` rather than the
        /// ordinary struct serializer.
        #[derive(Debug, Serialize)]
        struct Nested {
            inner: ImapCredentials,
        }

        let creds = ImapCredentials {
            host: "imap.example.com".into(),
            port: 993,
            username: "acme@opencompany.work".into(),
            password: SecretValue(FAKE_SECRET.to_string()),
        };
        let config = UnsuspectingConfig {
            label: "tenant mailbox".to_string(),
            primary: creds.clone(),
            optional: Some(creds.clone()),
            many: vec![creds.clone(), creds.clone()],
            by_name: BTreeMap::from([("acme".to_string(), creds.clone())]),
            nested: Nested {
                inner: creds.clone(),
            },
        };

        for rendering in [
            serde_json::to_string(&creds).expect("serializes"),
            serde_json::to_value(&creds)
                .expect("serializes")
                .to_string(),
            serde_json::to_string(&config).expect("serializes"),
            serde_json::to_value(&config)
                .expect("serializes")
                .to_string(),
        ] {
            assert!(!leaks(&rendering), "plaintext reached serde: {rendering}");
        }
        for rendering in [
            format!("{creds:?}"),
            format!("{creds:#?}"),
            format!("{config:?}"),
            format!("{config:#?}"),
        ] {
            assert!(!leaks(&rendering), "plaintext reached Debug: {rendering}");
        }

        // Still diagnosable: everything that is not the credential survives.
        let rendered = format!("{creds:?}");
        assert!(rendered.contains("imap.example.com"), "{rendered}");
        assert!(rendered.contains("993"), "{rendered}");
        assert!(rendered.contains("acme@opencompany.work"), "{rendered}");

        // And the credential itself is still reachable by the one named door,
        // so the poller can still log in.
        assert_eq!(creds.password.expose(), FAKE_SECRET);
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
        #[cfg(test)]
        if std::env::var_os("OPENCOMPANY_MAIL_TEST_INSECURE_TLS").is_some() {
            let config = tokio_rustls::rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(std::sync::Arc::new(TestOnlyCertificateVerifier))
                .with_no_client_auth();
            return Ok(tokio_rustls::TlsConnector::from(std::sync::Arc::new(
                config,
            )));
        }
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
            .login(&creds.username, creds.password.expose())
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
            // The parens are load-bearing: async-imap sends the query string
            // verbatim, and RFC 3501 requires a multi-item fetch to be a
            // parenthesized list. Without them the command is `UID FETCH <set>
            // UID BODY.PEEK[]` — two bare fetch-atts, which strict servers
            // (Stalwart) reject past the first, so `BODY[]` never comes back and
            // every message parses to an empty body. `UID` is requested
            // explicitly for clarity even though UID FETCH always echoes it.
            let mut fetches = session
                .uid_fetch(&set, "(UID BODY.PEEK[])")
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

// The CI-only Stalwart fixture creates its own certificate. Keep its explicit
// opt-out from certificate validation test-only: production builds always use
// the Mozilla root set configured above.
#[cfg(all(test, feature = "imap"))]
#[derive(Debug)]
struct TestOnlyCertificateVerifier;

#[cfg(all(test, feature = "imap"))]
impl tokio_rustls::rustls::client::danger::ServerCertVerifier for TestOnlyCertificateVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        use tokio_rustls::rustls::SignatureScheme;

        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
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

/// Live transport smoke test. Ignored by default — it talks to a real Stalwart
/// mailbox using credentials from the environment (the password is never in the
/// code). Sends a uniquely-tagged email FROM the configured mailbox TO itself,
/// then polls until it comes back, proving `LettreMailSender` (send),
/// `AsyncImapReceiver::fetch_new` (UID + BODY.PEEK), and `mark_seen` all work
/// end-to-end against real infrastructure.
///
/// Run it with the mailbox creds exported (SMTP_PORT 465 = implicit TLS):
/// ```sh
/// export OPENCOMPANY_MAIL_ADDRESS=alice@opencompany.work
/// export OPENCOMPANY_MAIL_SMTP_HOST=mail.opencompany.work
/// export OPENCOMPANY_MAIL_SMTP_PORT=465
/// export OPENCOMPANY_MAIL_IMAP_HOST=mail.opencompany.work
/// export OPENCOMPANY_MAIL_IMAP_PORT=993
/// export OPENCOMPANY_MAIL_USER=alice@opencompany.work
/// export OPENCOMPANY_MAIL_PASSWORD='…'
/// cargo test --features imap,smtp send_then_receive_roundtrip -- --ignored --nocapture
/// ```
#[cfg(all(test, feature = "imap", feature = "smtp"))]
mod live_smoke {
    use crate::ports::types::SecretValue;
    use crate::server::ops::imap::AsyncImapReceiver;
    use crate::server::ops::mailer::{
        MailCredentials, MailReceiver, MailSender, OutboundEmail, TenantMailboxConfig,
    };
    use crate::server::ops::smtp::LettreMailSender;

    #[tokio::test]
    #[ignore = "live: needs OPENCOMPANY_MAIL_* + a real Stalwart mailbox"]
    async fn send_then_receive_roundtrip() {
        let mut cfg = TenantMailboxConfig::from_env()
            .expect("OPENCOMPANY_MAIL_* parse failed")
            .expect("set OPENCOMPANY_MAIL_ADDRESS + the SMTP/IMAP vars first");

        // Stalwart's ephemeral CI container has a self-signed certificate.
        // This knob only exists in test builds, and leaves production's TLS
        // verification path unchanged. SMTP stays plaintext in that fixture;
        // IMAPS still performs a real TLS handshake via the test-only verifier.
        // The fixture's plaintext SMTP listener advertises no AUTH mechanism,
        // so the round trip is unauthenticated on that leg and authenticated on
        // IMAP — matching how the container is provisioned above.
        if std::env::var_os("OPENCOMPANY_MAIL_TEST_INSECURE_TLS").is_some() {
            cfg.smtp.security = crate::server::ops::smtp::SmtpSecurity::None;
            cfg.smtp.username.clear();
            cfg.smtp.password = SecretValue(String::new());
        }

        let token = format!(
            "SMOKE-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );
        eprintln!(
            "mailbox={} smtp={}:{}({:?}) imap={}:{}",
            cfg.address,
            cfg.smtp.host,
            cfg.smtp.port,
            cfg.smtp.security,
            cfg.imap.host,
            cfg.imap.port
        );

        // 1) Send to self.
        let email = OutboundEmail {
            to: cfg.address.clone(),
            subject: token.clone(),
            body: "workload transport smoke test".into(),
        };
        LettreMailSender
            .send(&MailCredentials::Smtp(cfg.smtp.clone()), &email)
            .await
            .expect("SMTP send failed");
        eprintln!("sent '{token}' -> {}", cfg.address);

        // 2) Poll the mailbox until it arrives (~up to 60s).
        let rx = AsyncImapReceiver;
        for attempt in 1..=12 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let msgs = rx.fetch_new(&cfg.imap).await.expect("IMAP fetch failed");
            eprintln!("poll {attempt}: {} unseen", msgs.len());
            if let Some(f) = msgs.iter().find(|f| f.email.subject.contains(&token)) {
                eprintln!(
                    "RECEIVED uid={} from={} subject='{}'",
                    f.uid, f.email.from_email, f.email.subject
                );
                rx.mark_seen(&cfg.imap, &[f.uid])
                    .await
                    .expect("mark_seen failed");
                eprintln!("marked uid {} Seen — round-trip OK", f.uid);
                return;
            }
        }
        panic!("round-trip message '{token}' not received within ~60s");
    }
}
