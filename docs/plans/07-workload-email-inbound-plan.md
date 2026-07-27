# WS7 Inbound — IMAP Receive: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add IMAP-poll receiving so a running company reads its own `<slug>@opencompany.work` mailbox and each new message drives an agent cycle — reusing the existing `InboxStore` + `WebhookReceived` path.

**Architecture:** A `MailReceiver` trait (mock + a feature-gated `async-imap` impl), a `MailboxPoller` structured like the existing `CompanyScheduler`, and a shared `file_and_notify` helper factored out of the existing webhook `ingest()`. Outbound send is a separate plan (deferred pending the openhuman effect-model spike).

**Tech Stack:** Rust 2024, `async_trait`, `tokio`, `async-imap` + `mail-parser` (new, behind a new `imap` Cargo feature), the crate error type `OpenCompanyError`.

**Design source:** [`07-workload-email-send-receive.md`](07-workload-email-send-receive.md) — this plan implements its **inbound** half only.

## Global Constraints

- Before **every** commit: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build --all-targets`, `cargo test` must pass (workload `CLAUDE.md`). **Toolchain note:** this machine's default `stable` is 1.93.0 and may fail to build a transitive dep (`cfg_select`); if a plain `cargo build` fails on a dependency, use `cargo +1.96.1 …` (installed) as the manager side did. Confirm which toolchain builds cleanly before Task 1 and use it throughout.
- All async traits use the external **`#[async_trait]`** macro (never native `async fn` in trait) — every port here is stored as `Arc<dyn …>`.
- Errors use `OpenCompanyError` variants (`Config(String)`, `Store(String)`, `InvalidRequest(String)`, `Serde(#[from])`). No `anyhow`.
- **Feature-gate the network crate:** `async-imap` + `mail-parser` link only under a new `imap` feature (mirroring how `lettre` is gated behind `smtp`). The `MailReceiver` trait, `InboundEmail`, `ImapCredentials`, `RecordingMailReceiver`, `MailboxPoller`, and the shared helper all compile in the default build; only `AsyncImapReceiver` + the RFC822 parse fn are `#[cfg(feature = "imap")]`.
- **Secrets:** the IMAP password is a secret — never logged / never `#[derive(Debug)]`-printed in a way that leaks it (follow `SmtpCredentials`' handling; if a `Debug` would print the password, hand-write it like `MailCredentials` does at `mailer.rs:120`).
- Reuse, don't reinvent: `EmailRecord`/`InboxStore` (`src/ports/inbox.rs`), `CompanyEvent::WebhookReceived` (`src/ports/types.rs:233`), `Clock`/spawn pattern (`src/runtime/scheduler.rs`), `local_part` (`src/server/ops/smtp.rs:249`).

## File Structure

- Modify: `Cargo.toml` — new `imap` feature + optional deps.
- Modify: `src/server/ops/mailer.rs` — `InboundEmail`, `MailReceiver` trait, `RecordingMailReceiver`, `TenantMailboxConfig` + `from_env`.
- Create: `src/server/ops/imap.rs` — `ImapCredentials` (always compiled) + `AsyncImapReceiver` + `parse_message` (feature-gated).
- Modify: `src/server/ops/mod.rs` — declare `pub mod imap;` and re-exports.
- Modify: `src/server/ops/inbox.rs` — extract `file_and_notify`; `ingest()` calls it.
- Create: `src/runtime/mailbox_poller.rs` — `MailboxPoller`.
- Modify: `src/runtime/mod.rs` — `pub mod mailbox_poller;`.
- Modify: `src/bin/opencompany.rs` — construct config + receiver, spawn a poller per company.

---

### Task 1: `imap` Cargo feature + deps

**Files:** Modify `Cargo.toml`

**Interfaces:** Produces the `imap` feature enabling `async-imap` + `mail-parser`.

- [ ] **Step 1: Add optional deps** to `[dependencies]` (near the `lettre` line, matching its comment style):

```toml
# IMAP inbound transport + RFC822 parsing for per-teammate mail receiving. Only
# link under the `imap` feature; the `MailReceiver` trait + offline mock compile
# without it. Pure-Rust rustls TLS so no system libs are required.
async-imap = { version = "0.10", default-features = false, features = ["runtime-tokio"], optional = true }
mail-parser = { version = "0.9", optional = true }
```

- [ ] **Step 2: Add the feature** to `[features]` (beside `smtp`):

```toml
# Real IMAP receive for the email surface. The `MailReceiver` trait and the
# offline `RecordingMailReceiver` compile without this; only `AsyncImapReceiver`
# and the RFC822 parser are gated here.
imap = ["dep:async-imap", "dep:mail-parser"]
```

- [ ] **Step 3: Verify both feature states resolve**

Run: `cargo build --all-targets` and `cargo build --all-targets --features imap`
Expected: both succeed (async-imap/mail-parser download + compile under the feature). If the default toolchain fails on a dep, switch to `cargo +1.96.1` (see Global Constraints) and use it for all later steps.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build(email): add imap feature (async-imap + mail-parser)"
```

---

### Task 2: `ImapCredentials` + `InboundEmail` + `MailReceiver` trait + mock

**Files:** Create `src/server/ops/imap.rs`; Modify `src/server/ops/mailer.rs`, `src/server/ops/mod.rs`

**Interfaces:**
- Produces: `ImapCredentials { host, port, username, password }`; `InboundEmail { from_name, from_email, subject, body }`; `trait MailReceiver { async fn fetch_new(&self, creds: &ImapCredentials) -> Result<Vec<InboundEmail>, OpenCompanyError> }`; `RecordingMailReceiver` (queued messages, records call count).

- [ ] **Step 1: Create `src/server/ops/imap.rs`** with the always-compiled credentials type (network client comes in Task 4):

```rust
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
```

- [ ] **Step 2: Register the module** — add to `src/server/ops/mod.rs`: `pub mod imap;`

- [ ] **Step 3: Add `InboundEmail` + `MailReceiver` + mock** to `src/server/ops/mailer.rs` (near `MailSender`), with the failing test:

```rust
use std::sync::Mutex;
use crate::server::ops::imap::ImapCredentials;

/// One inbound message produced by a [`MailReceiver`]. Plain-text body (v1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundEmail {
    pub from_name: String,
    pub from_email: String,
    pub subject: String,
    pub body: String,
}

/// The inbound-fetch seam. Implementations fetch *new* (unseen) messages and
/// mark them seen, so a subsequent call returns only newer mail. Mockable so the
/// poller is exercised offline; the real transport is feature-gated.
#[async_trait]
pub trait MailReceiver: Send + Sync {
    async fn fetch_new(
        &self,
        creds: &ImapCredentials,
    ) -> Result<Vec<InboundEmail>, OpenCompanyError>;
}

/// Offline mock: returns queued batches, one per `fetch_new` call, and counts calls.
pub struct RecordingMailReceiver {
    batches: Mutex<std::collections::VecDeque<Vec<InboundEmail>>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl RecordingMailReceiver {
    pub fn new() -> Self {
        Self { batches: Mutex::new(std::collections::VecDeque::new()), calls: Default::default() }
    }
    /// Queue a batch to be returned by the next `fetch_new`.
    pub fn push_batch(&self, batch: Vec<InboundEmail>) {
        self.batches.lock().expect("poisoned").push_back(batch);
    }
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for RecordingMailReceiver {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl MailReceiver for RecordingMailReceiver {
    async fn fetch_new(&self, _creds: &ImapCredentials) -> Result<Vec<InboundEmail>, OpenCompanyError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self.batches.lock().expect("poisoned").pop_front().unwrap_or_default())
    }
}
```

Test (in `mailer.rs`'s test module):

```rust
#[tokio::test]
async fn recording_receiver_returns_queued_batches_in_order() {
    let creds = ImapCredentials { host: "h".into(), port: 993, username: "u".into(), password: "p".into() };
    let rx = RecordingMailReceiver::new();
    rx.push_batch(vec![InboundEmail { from_name: "A".into(), from_email: "a@x".into(), subject: "s".into(), body: "b".into() }]);
    assert_eq!(rx.fetch_new(&creds).await.unwrap().len(), 1);
    assert_eq!(rx.fetch_new(&creds).await.unwrap().len(), 0); // drained
    assert_eq!(rx.calls(), 2);
}
```

- [ ] **Step 4: Run + gate**

Run: `cargo test mailer:: && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/server/ops/imap.rs src/server/ops/mailer.rs src/server/ops/mod.rs
git commit -m "feat(email): MailReceiver trait + ImapCredentials + offline mock"
```

---

### Task 3: `TenantMailboxConfig::from_env`

**Files:** Modify `src/server/ops/mailer.rs`

**Interfaces:**
- Consumes: `SmtpCredentials` (`smtp.rs`), `ImapCredentials` (Task 2).
- Produces: `TenantMailboxConfig { address, smtp: SmtpCredentials, imap: ImapCredentials }`; `pub fn from_env() -> Result<Option<Self>, OpenCompanyError>` reading the manager-injected `OPENCOMPANY_MAIL_ADDRESS/SMTP_HOST/SMTP_PORT/IMAP_HOST/IMAP_PORT/USER/PASSWORD`. `None` when `OPENCOMPANY_MAIL_ADDRESS` is unset; a partial set is an error.

- [ ] **Step 1: Write the failing test** (uses a scoped env guard; run serially):

```rust
#[test]
fn tenant_mailbox_config_parses_injected_env() {
    // Serialize env access; set the 7 injected vars.
    let _g = ENV_LOCK.lock().unwrap();
    for (k, v) in [
        ("OPENCOMPANY_MAIL_ADDRESS", "acme@opencompany.work"),
        ("OPENCOMPANY_MAIL_SMTP_HOST", "mail.opencompany.work"),
        ("OPENCOMPANY_MAIL_SMTP_PORT", "465"),
        ("OPENCOMPANY_MAIL_IMAP_HOST", "mail.opencompany.work"),
        ("OPENCOMPANY_MAIL_IMAP_PORT", "993"),
        ("OPENCOMPANY_MAIL_USER", "acme@opencompany.work"),
        ("OPENCOMPANY_MAIL_PASSWORD", "secret"),
    ] { unsafe { std::env::set_var(k, v) }; }

    let cfg = TenantMailboxConfig::from_env().unwrap().expect("configured");
    assert_eq!(cfg.address, "acme@opencompany.work");
    assert_eq!(cfg.imap.host, "mail.opencompany.work");
    assert_eq!(cfg.imap.port, 993);
    assert_eq!(cfg.smtp.from_email, "acme@opencompany.work");

    for k in ["OPENCOMPANY_MAIL_ADDRESS","OPENCOMPANY_MAIL_SMTP_HOST","OPENCOMPANY_MAIL_SMTP_PORT","OPENCOMPANY_MAIL_IMAP_HOST","OPENCOMPANY_MAIL_IMAP_PORT","OPENCOMPANY_MAIL_USER","OPENCOMPANY_MAIL_PASSWORD"] {
        unsafe { std::env::remove_var(k) };
    }
}
```

Add near the test module: `static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());` (guard shared by env-reading tests). Verify FAIL: `cargo test tenant_mailbox_config_parses_injected_env` → does not compile / not found.

- [ ] **Step 2: Implement `TenantMailboxConfig`** (mirror `MailConfig::from_env`'s `var`/`missing` idiom at `mailer.rs:181`; note the separate host-level `OPENCOMPANY_MAIL_HOST/PORT/...` remain for platform mail — document the split):

```rust
use crate::server::ops::smtp::SmtpCredentials;

/// A managed tenant's OWN mailbox identity, injected by the manager as
/// `OPENCOMPANY_MAIL_*`. Distinct from the host-level `OPENCOMPANY_MAIL_HOST/...`
/// platform-mail read by `MailConfig` (login links). Seeds the company's SMTP
/// send credentials AND the IMAP poller config.
#[derive(Clone, Debug)]
pub struct TenantMailboxConfig {
    pub address: String,
    pub smtp: SmtpCredentials,
    pub imap: ImapCredentials,
}

impl TenantMailboxConfig {
    /// `Ok(None)` when unconfigured (no `OPENCOMPANY_MAIL_ADDRESS`); a *partial*
    /// injection is a hard error.
    pub fn from_env() -> Result<Option<Self>, OpenCompanyError> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let Some(address) = var("OPENCOMPANY_MAIL_ADDRESS") else { return Ok(None) };
        let need = |k: &str| var(k).ok_or_else(|| OpenCompanyError::Config(
            format!("{k} is required when OPENCOMPANY_MAIL_ADDRESS is set")));
        let port = |k: &str| -> Result<u16, OpenCompanyError> {
            need(k)?.parse::<u16>().map_err(|_| OpenCompanyError::Config(format!("{k} must be a port number")))
        };
        let user = need("OPENCOMPANY_MAIL_USER")?;
        let password = need("OPENCOMPANY_MAIL_PASSWORD")?;
        let smtp = SmtpCredentials {
            host: need("OPENCOMPANY_MAIL_SMTP_HOST")?,
            port: port("OPENCOMPANY_MAIL_SMTP_PORT")?,
            security: crate::server::ops::smtp::SmtpSecurity::default(),
            username: user.clone(),
            password: password.clone(),
            from_name: String::new(),
            from_email: address.clone(),
        };
        let imap = ImapCredentials {
            host: need("OPENCOMPANY_MAIL_IMAP_HOST")?,
            port: port("OPENCOMPANY_MAIL_IMAP_PORT")?,
            username: user,
            password,
        };
        Ok(Some(Self { address, smtp, imap }))
    }
}
```

- [ ] **Step 3: Add the partial-config + absent tests**

```rust
#[test]
fn tenant_mailbox_config_absent_is_none() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe { std::env::remove_var("OPENCOMPANY_MAIL_ADDRESS") };
    assert!(TenantMailboxConfig::from_env().unwrap().is_none());
}

#[test]
fn tenant_mailbox_config_partial_is_error() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe { std::env::set_var("OPENCOMPANY_MAIL_ADDRESS", "acme@opencompany.work") };
    unsafe { std::env::remove_var("OPENCOMPANY_MAIL_PASSWORD") };
    assert!(TenantMailboxConfig::from_env().is_err());
    unsafe { std::env::remove_var("OPENCOMPANY_MAIL_ADDRESS") };
}
```

- [ ] **Step 4: Run + gate**

Run: `cargo test mailer:: -- --test-threads=1 && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS (env tests run single-threaded via the shared `ENV_LOCK`).

- [ ] **Step 5: Commit**

```bash
git add src/server/ops/mailer.rs
git commit -m "feat(email): TenantMailboxConfig::from_env for injected per-tenant creds"
```

---

### Task 4: `AsyncImapReceiver` + `parse_message` (feature-gated)

**Files:** Modify `src/server/ops/imap.rs`

**Interfaces:** Produces `AsyncImapReceiver` (`impl MailReceiver`, `#[cfg(feature="imap")]`) and a testable pure `parse_message(raw: &[u8]) -> InboundEmail`.

- [ ] **Step 1: Write the failing parse test** (feature-gated; run with `--features imap`):

```rust
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
```

Run: `cargo test --features imap parse_message` → FAIL (undefined).

- [ ] **Step 2: Implement `parse_message` + `AsyncImapReceiver`** in `imap.rs`:

```rust
#[cfg(feature = "imap")]
use crate::server::ops::mailer::{InboundEmail, MailReceiver};
#[cfg(feature = "imap")]
use crate::error::OpenCompanyError;

/// Parse one RFC822 message into an `InboundEmail` (plain-text body, v1).
#[cfg(feature = "imap")]
pub(crate) fn parse_message(raw: &[u8]) -> InboundEmail {
    use mail_parser::MessageParser;
    let parsed = MessageParser::default().parse(raw);
    let (from_name, from_email) = parsed
        .as_ref()
        .and_then(|m| m.from())
        .and_then(|a| a.first())
        .map(|addr| (
            addr.name().unwrap_or_default().to_string(),
            addr.address().unwrap_or_default().to_string(),
        ))
        .unwrap_or_default();
    let subject = parsed.as_ref().and_then(|m| m.subject()).unwrap_or_default().to_string();
    let body = parsed.as_ref().and_then(|m| m.body_text(0)).map(|c| c.to_string()).unwrap_or_default();
    InboundEmail { from_name, from_email, subject, body }
}

/// Real IMAP poller: connect over TLS, SELECT INBOX, SEARCH UNSEEN, FETCH,
/// parse, then mark the fetched messages `\Seen`.
#[cfg(feature = "imap")]
pub struct AsyncImapReceiver;

#[cfg(feature = "imap")]
#[async_trait::async_trait]
impl MailReceiver for AsyncImapReceiver {
    async fn fetch_new(&self, creds: &ImapCredentials) -> Result<Vec<InboundEmail>, OpenCompanyError> {
        use futures::TryStreamExt;
        let stream = async_imap::connect_tls((creds.host.as_str(), creds.port))
            .await
            .map_err(|e| OpenCompanyError::Store(format!("imap connect: {e}")))?;
        let mut session = stream
            .login(&creds.username, &creds.password)
            .await
            .map_err(|(e, _)| OpenCompanyError::Store(format!("imap login: {e}")))?;
        session.select("INBOX").await.map_err(|e| OpenCompanyError::Store(format!("imap select: {e}")))?;
        let unseen = session.search("UNSEEN").await.map_err(|e| OpenCompanyError::Store(format!("imap search: {e}")))?;
        let mut out = Vec::new();
        if !unseen.is_empty() {
            let set = unseen.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
            // RFC822 marks \Seen on fetch; that's the intended dedup (Decision 5).
            let mut fetches = session.fetch(&set, "RFC822").await
                .map_err(|e| OpenCompanyError::Store(format!("imap fetch: {e}")))?;
            while let Some(f) = fetches.try_next().await
                .map_err(|e| OpenCompanyError::Store(format!("imap fetch stream: {e}")))? {
                if let Some(body) = f.body() { out.push(parse_message(body)); }
            }
        }
        let _ = session.logout().await;
        Ok(out)
    }
}
```

Add `futures` if not already a dep (check `Cargo.toml`; the crate already uses `BoxStream` per the channel port, so `futures` is present — confirm and only add under the `imap` feature if missing).

- [ ] **Step 3: Run the parse test + both build states**

Run: `cargo test --features imap parse_message` (PASS) and `cargo build --all-targets` (default, no imap) and `cargo build --all-targets --features imap`, then `cargo clippy --all-targets --features imap -- -D warnings` and `cargo fmt --all -- --check`.
Expected: all PASS. (The live `fetch_new` path is validated by the poller's mock tests in Task 6 + a manual/integration send later; there is no offline IMAP server here.)

- [ ] **Step 4: Commit**

```bash
git add src/server/ops/imap.rs Cargo.toml Cargo.lock
git commit -m "feat(email): AsyncImapReceiver + RFC822 parse (imap feature)"
```

---

### Task 5: Shared `file_and_notify` helper

**Files:** Modify `src/server/ops/inbox.rs`

**Interfaces:** Produces `pub(crate) async fn file_and_notify(runtime: &CompanyRuntime, to: &str, record: EmailRecord) -> Result<()>` — appends the record then, if running, fires `WebhookReceived{channel:"email"}`. `ingest()` is refactored to call it (behavior-preserving).

- [ ] **Step 1: Extract the helper** (the tail of `ingest()` at `inbox.rs:139-159`):

```rust
/// File an inbound email and, if the company is running, drive one cycle so the
/// addressed teammate can act on it. Shared by the ingest webhook and the IMAP
/// poller. `to` is the full recipient address (for the event body); the record
/// already carries the local-part `inbox`.
pub(crate) async fn file_and_notify(
    runtime: &CompanyRuntime,
    to: &str,
    record: EmailRecord,
) -> crate::Result<()> {
    runtime.inbox().append(runtime.id(), &record).await?;
    if runtime.ensure_running().await.is_ok() {
        let event = CompanyEvent::WebhookReceived {
            channel: "email".to_string(),
            body: serde_json::json!({
                "from": record.from_email,
                "to": to,
                "inbox": record.inbox,
                "subject": record.subject,
                "body": record.body,
            }),
        };
        if let Err(err) = runtime.run_cycle(vec![event]).await {
            tracing::warn!(company = %runtime.id(), "email cycle failed: {err}");
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Rewrite `ingest()`'s tail** to build the `EmailRecord` then call the helper, preserving the current behavior (return `ApiError` if `file_and_notify` errors on append):

```rust
    let record = EmailRecord {
        id: generate_id(),
        inbox: inbox.clone(),
        from_name: String::new(),
        from_email: email.from.clone(),
        subject: email.subject.clone(),
        body: email.body.clone(),
        at_millis: now_millis(),
        read: false,
        outbound: false,
    };
    if let Err(err) = file_and_notify(&runtime, &email.to, record).await {
        return crate::server::error::ApiError(err).into_response();
    }
    (StatusCode::ACCEPTED, Json(IngestAck { ok: true, inbox })).into_response()
```

- [ ] **Step 3: Run the existing inbox tests** (they must still pass — behavior preserved)

Run: `cargo test inbox:: && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS (existing ingest tests unchanged in behavior).

- [ ] **Step 4: Commit**

```bash
git add src/server/ops/inbox.rs
git commit -m "refactor(email): extract file_and_notify shared by ingest + poller"
```

---

### Task 6: `MailboxPoller`

**Files:** Create `src/runtime/mailbox_poller.rs`; Modify `src/runtime/mod.rs`

**Interfaces:**
- Consumes: `MailReceiver`, `ImapCredentials`, `file_and_notify`, `Clock` (`scheduler.rs`), `CompanyRuntime`.
- Produces: `MailboxPoller::new(runtime, receiver, creds, address, interval_secs)`, `async fn tick(&self) -> Result<usize>`, `fn spawn(self, shutdown: Arc<Notify>) -> JoinHandle<()>`.

- [ ] **Step 1: Write the failing test** — one tick with a mock receiver files N records + fires N cycles:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // Build a CompanyRuntime in the "running" state via the existing test
    // harness/builder used by scheduler.rs tests (mirror its setup), inject a
    // RecordingMailReceiver with one queued batch of 2 messages.
    #[tokio::test]
    async fn tick_files_and_notifies_each_message() {
        let runtime = /* test CompanyRuntime, running */;
        let rx = std::sync::Arc::new(RecordingMailReceiver::new());
        rx.push_batch(vec![
            InboundEmail { from_name: "A".into(), from_email: "a@x".into(), subject: "s1".into(), body: "b1".into() },
            InboundEmail { from_name: "B".into(), from_email: "b@x".into(), subject: "s2".into(), body: "b2".into() },
        ]);
        let creds = ImapCredentials { host: "h".into(), port: 993, username: "u".into(), password: "p".into() };
        let poller = MailboxPoller::new(runtime.clone(), rx.clone(), creds, "acme@opencompany.work".into(), 60);
        let n = poller.tick().await.unwrap();
        assert_eq!(n, 2);
        // both filed to the inbox
        assert_eq!(runtime.inbox().messages(runtime.id(), "acme", 10, 0).await.unwrap().len(), 2);
    }
}
```

(Model the `CompanyRuntime` construction on `scheduler.rs`'s own tests — reuse the same in-memory builder + a `"running"` lifecycle.)

Run: `cargo test mailbox_poller` → FAIL.

- [ ] **Step 2: Implement `MailboxPoller`**:

```rust
//! Per-company IMAP poller. Structured like `CompanyScheduler`: an injectable
//! interval loop that, per tick, fetches new mail via a `MailReceiver` and files
//! it through the shared `file_and_notify`. Skips while the company is asleep
//! (scale-to-zero: unseen mail waits in Stalwart and is picked up on wake).
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::company::runtime::CompanyRuntime;
use crate::server::ops::imap::ImapCredentials;
use crate::server::ops::inbox::file_and_notify;
use crate::server::ops::mailer::MailReceiver;
use crate::ports::inbox::EmailRecord;
use crate::{now_millis, generate_id}; // adjust import paths to the crate's helpers
use crate::server::ops::smtp::local_part;

pub struct MailboxPoller {
    runtime: Arc<CompanyRuntime>,
    receiver: Arc<dyn MailReceiver>,
    creds: ImapCredentials,
    address: String,
    interval: Duration,
}

impl MailboxPoller {
    pub fn new(
        runtime: Arc<CompanyRuntime>,
        receiver: Arc<dyn MailReceiver>,
        creds: ImapCredentials,
        address: String,
        interval_secs: u64,
    ) -> Self {
        Self { runtime, receiver, creds, address, interval: Duration::from_secs(interval_secs.max(1)) }
    }

    /// Fetch new mail and file each message. Returns the count filed. Skips (0)
    /// when the company is not running.
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

    /// Spawn the interval loop; stops on `shutdown`.
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
```

Adjust the exact import paths for `now_millis`/`generate_id`/`local_part` to wherever the crate exposes them (grep: `fn now_millis`, `fn generate_id`). Make `local_part` reachable (it's `pub(crate)` in `smtp.rs`).

- [ ] **Step 3: Register + run**

Add `pub mod mailbox_poller;` to `src/runtime/mod.rs`.
Run: `cargo test mailbox_poller && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/runtime/mailbox_poller.rs src/runtime/mod.rs
git commit -m "feat(email): MailboxPoller (IMAP poll -> file_and_notify)"
```

---

### Task 7: Wire the poller into the serve command

**Files:** Modify `src/bin/opencompany.rs`

**Interfaces:** Consumes `TenantMailboxConfig::from_env`, `AsyncImapReceiver`/`MailReceiver`, `MailboxPoller`. Starts one poller per running company under the shared `shutdown`.

- [ ] **Step 1: Add a start helper** near `spawn_scheduler` (`:209`):

```rust
fn spawn_mailbox_poller(
    state: &AppState,
    id: &str,
    shutdown: &Arc<Notify>,
    handles: &mut Vec<JoinHandle<()>>,
) {
    // Only when the manager injected this tenant's mailbox creds, and only when
    // the imap transport is compiled in.
    let cfg = match opencompany::server::ops::mailer::TenantMailboxConfig::from_env() {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return,
        Err(err) => { eprintln!("mailbox config error: {err}"); return; }
    };
    #[cfg(feature = "imap")]
    {
        let Some(runtime) = state.registry().get(&CompanyId::new(id)) else { return };
        let receiver: Arc<dyn opencompany::server::ops::mailer::MailReceiver> =
            Arc::new(opencompany::server::ops::imap::AsyncImapReceiver);
        let interval = std::env::var("OPENCOMPANY_MAIL_POLL_SECONDS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(60);
        let poller = opencompany::runtime::mailbox_poller::MailboxPoller::new(
            runtime, receiver, cfg.imap.clone(), cfg.address.clone(), interval);
        handles.push(poller.spawn(shutdown.clone()));
    }
    #[cfg(not(feature = "imap"))]
    { let _ = (state, id, shutdown, handles, cfg); }
}
```

- [ ] **Step 2: Call it** in the `for dir in &companies` loop right after `spawn_scheduler(&state, &id, &schedules, &shutdown)` (`:552-574`):

```rust
        spawn_scheduler(&state, &id, &schedules, &shutdown);
        spawn_mailbox_poller(&state, &id, &shutdown, &mut scheduler_handles);
```

- [ ] **Step 3: Build both feature states + full suite**

Run: `cargo build --all-targets` and `cargo build --all-targets --features imap` and `cargo test` and `cargo clippy --all-targets --features imap -- -D warnings` and `cargo fmt --all -- --check`.
Expected: all PASS.

- [ ] **Step 4: Commit**

```bash
git add src/bin/opencompany.rs
git commit -m "feat(email): start a per-company IMAP poller in serve"
```

---

## Post-implementation (not code tasks)

- **Live smoke test** (with `--features imap` + a real tenant, `OPENCOMPANY_MAIL_*` injected): send mail to `<slug>@opencompany.work`; confirm the poller files an `EmailRecord` and drives a cycle.
- **Outbound** (`send_email`) is the separate deferred half — pending the `vendor/openhuman` effect-model spike; it will reuse `MailSender`/`SmtpCredentials`/`file_and_notify`-adjacent `record_outbound` and `TenantMailboxConfig.smtp`.
- **Docs:** update `docs/spec/runtime/ports.md` / `docs/modules/` to note the IMAP receive path alongside the webhook ingest.
