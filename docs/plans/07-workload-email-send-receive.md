# WS7: Workload Email — Agent Send + IMAP Receive

**Status:** Design (approved for planning) · **Date:** 2026-07-28
**Builds on:** [`06-connections-domain-email.md`](06-connections-domain-email.md) — completes its two deferred subtasks (the `send_email` tool and inbound receiving) and adds IMAP polling.
**Platform context:** the manager (Phase B.1) provisions a per-tenant Stalwart mailbox `<slug>@opencompany.work` and injects `OPENCOMPANY_MAIL_*` credentials into this workload. This spec is the workload half — sub-project 2 of Phase B.

---

## 1. Goal

Give the running company **full conversational email**: the agent can **send** mail from its own `<slug>@opencompany.work` address, and mail arriving at that address is **received** into its inbox and drives an agent cycle — a complete round-trip, with no cross-tenant access.

**Success =** the agent invokes `send_email` and a signed message leaves the mailbox (subject to approval policy); and a reply to that message is polled from IMAP, filed to the inbox, and surfaced to the agent as a cycle event.

## 2. Locked decisions

| # | Decision | Choice |
|---|---|---|
| 1 | Scope | **Both** outbound send + inbound receive (full round-trip). |
| 2 | Inbound mechanism | **IMAP poll** — the workload reads its own mailbox with the injected IMAP creds. Scale-to-zero friendly (asleep ⇒ mail waits in Stalwart, picked up on wake). Not webhook-push. |
| 3 | Send authorization | `send_email` **routes through the approval policy** (external-publish effect via `emit_effect`); default requires operator approval, a company may opt into auto-send. |
| 4 | Body format (v1) | **Plain text only.** Attachments, HTML, threading are out of scope. |
| 5 | Dedup | Fetch `UNSEEN`, ingest, then mark `\Seen` server-side (the mailbox is the agent's). |
| 6 | Poll interval | `OPENCOMPANY_MAIL_POLL_SECONDS`, default `60`. |

## 3. Existing surfaces reused (do NOT reinvent)

Verified against the current tree:

- **`InboxStore`** (`src/ports/inbox.rs:61`) — `EmailRecord { id, inbox, from_name, from_email, subject, body, at_millis, read, outbound }` (`:36`) + `append` / `messages` / `mark_read`. Backends already exist (fs/sqlite/mongo), wired as `CompanyRuntime.inbox` (`src/company/runtime.rs:80`).
- **`MailSender`** (`src/server/ops/mailer.rs:147`) — `async fn send(&self, creds: &MailCredentials, email: &OutboundEmail)`. Real `LettreMailSender` (`src/server/ops/smtp.rs:272`, `smtp` feature), mock `RecordingMailSender` (`:244`). `MailCredentials::Smtp(SmtpCredentials)`, `OutboundEmail { to, subject, body }`.
- **Inbound event path** — `src/server/ops/inbox.rs::ingest()` (`:93-162`) appends an `EmailRecord` then, if the company is running, fires `run_cycle(vec![CompanyEvent::WebhookReceived { channel: "email", body }])`. `CompanyEvent::WebhookReceived` (`src/ports/types.rs`) is the closed event the agent already handles.
- **Poller precedent** — `CompanyScheduler` (`src/runtime/scheduler.rs:38-166`) ticks against an injectable `Clock`; the structural template for the mailbox poller.
- **Tool seam** — `trait ToolProvider { catalog, invoke }` (`src/ports/tools.rs:14`); `BuiltinToolProvider` currently wraps only `feedback` (`src/runtime/builder.rs:566-571`) — `send_email` joins it here.
- **Approval seam** — `CycleHost::emit_effect` (`src/ports/brain.rs`) → `perform_effect` (`src/runtime/cycle.rs:246-277`) already gates external effects through the policy.
- **Deps** — `lettre` present (`smtp` feature). **No IMAP crate, no mail-parser** — added here.

## 4. Design

### 4.1 Config — consume the manager-injected per-tenant creds

Add `TenantMailboxConfig::from_env()` in `src/server/ops/mailer.rs`, following the existing `MailConfig::from_env()` `var()` + `missing()` idiom (`:181-238`) where a *partial* config is a hard error.

Reads: `OPENCOMPANY_MAIL_ADDRESS`, `OPENCOMPANY_MAIL_SMTP_HOST`, `OPENCOMPANY_MAIL_SMTP_PORT`, `OPENCOMPANY_MAIL_IMAP_HOST`, `OPENCOMPANY_MAIL_IMAP_PORT`, `OPENCOMPANY_MAIL_USER`, `OPENCOMPANY_MAIL_PASSWORD`.

> **Naming note:** these are the **company's own mailbox** (its sending identity). They are distinct from the pre-existing host-level `OPENCOMPANY_MAIL_HOST/PORT/PROVIDER/FROM_*/USERNAME/PASSWORD/SECURITY` that `MailConfig` reads for *platform* mail (login links). Keep the two structs separate; document the split at both sites.

When present (managed tenant): seed the company's `MailCredentials::Smtp` (host/port/user/pass, `from = ADDRESS`) so `send_email` works without a manual `PUT /smtp`, **and** produce the poller's IMAP config. Injected env is authoritative for a managed tenant; the manual `PUT …/smtp` path (`src/server/ops/smtp.rs:148`) remains for self-hosted companies. When absent, email features are simply inactive (backwards compatible).

### 4.2 Outbound — the `send_email` builtin tool

A new builtin tool (mirroring `feedback`, `src/feedback/tool.rs`), registered in `BuiltinToolProvider`:

- **`ToolSpec`**: name `send_email`, args `{ to: string, subject: string, body: string }`.
- **`invoke`** does not send directly — it **emits an external-publish `Effect`** (payload carrying to/subject/body) via the cycle host, so the approval policy decides (Decision 3). This matches how other external effects are gated (`perform_effect`, `cycle.rs:246-277`).
- **On approved execution**: call `MailSender::send(smtp_creds, OutboundEmail { to, subject, body })`; on success, append an outbound `EmailRecord { outbound: true, from_email: <own address>, … }` to `InboxStore` (reusing the `record_outbound` pattern, `src/server/ops/smtp.rs:230-245`).
- `From` is always the tenant's own `OPENCOMPANY_MAIL_ADDRESS` (the agent cannot spoof another sender).

### 4.3 Inbound — the IMAP poller

New `MailboxPoller` (new module, e.g. `src/runtime/mailbox_poller.rs`), one per running company that has IMAP config, structured like `CompanyScheduler`:

- Holds an injectable `Clock` (test seam) and a `dyn MailReceiver` (§4.4).
- **Per tick**: `MailReceiver::fetch_new()` → for each `InboundEmail`, build `EmailRecord { inbound, from_name, from_email, subject, body, … }` and call the shared **`file_and_notify(runtime, record)`** helper (§4.5).
- The receiver marks fetched messages `\Seen` so the next tick skips them (Decision 5).
- **Lifecycle**: started when the company starts running, stopped on sleep/suspend (mirrors scheduler lifecycle). Asleep ⇒ no poll ⇒ mail accumulates unseen in Stalwart ⇒ ingested on next wake.

### 4.4 Transport — `MailReceiver` trait + deps

Add alongside `MailSender` in `mailer.rs`:

```
trait MailReceiver: Send + Sync {
    async fn fetch_new(&self, creds: &ImapCredentials) -> Result<Vec<InboundEmail>>;
}
InboundEmail { from_name, from_email, subject, body }   // plain-text body (v1)
```

- Real impl `AsyncImapReceiver` in a new `src/server/ops/imap.rs` (beside `smtp.rs`), behind a **new `imap` Cargo feature** mirroring `smtp`: adds `async-imap` (pure-Rust, tokio + rustls) + `mail-parser`. Connects, `SELECT INBOX`, `SEARCH UNSEEN`, `FETCH`, parses headers/text body, marks `\Seen`.
- Mock `RecordingMailReceiver` (always available, like `RecordingMailSender`) returns queued messages — the poller is fully testable without a live server.

### 4.5 Shared filing helper (DRY)

Factor the "append EmailRecord + fire run_cycle" tail of `src/server/ops/inbox.rs::ingest()` (`:127-159`) into a shared helper:

```
async fn file_and_notify(runtime: &CompanyRuntime, record: EmailRecord) -> Result<()>
```

Both the existing `/inboxes/ingest` webhook route and the new poller call it, so inbound filing + the `WebhookReceived{channel:"email"}` cycle trigger live in exactly one place.

### 4.6 Wiring

In `src/bin/opencompany.rs` / `ConnectionsRuntime` (`:298-317`, where `LettreMailSender` + `MailConfig` are already injected):
- Build `TenantMailboxConfig::from_env()`; if present, seed company SMTP creds and construct the `MailReceiver`.
- Register the `send_email` builtin tool.
- Start a `MailboxPoller` per running company with IMAP config; tie its lifecycle to company run/sleep.

## 5. Test plan

- **T1 send (gated):** agent calls `send_email`; with the default policy, an approval effect is raised and **no** mail is sent until approved; with an auto-approve policy, `MailSender::send` is called and an outbound `EmailRecord` is recorded. (Mock `MailSender`.)
- **T2 receive:** `RecordingMailReceiver` returns 2 messages; one poller tick appends 2 inbound `EmailRecord`s and fires `run_cycle` with a `WebhookReceived{channel:"email"}` per message (injectable `Clock`).
- **T3 config:** `TenantMailboxConfig::from_env` parses the 7 injected vars; a partial set (address without password) is a hard error; absent ⇒ `None` (feature off).
- **T4 helper:** `file_and_notify` is exercised by both the webhook route and the poller (one code path).
- **T5 round-trip (integration-style, mocks):** inbound message → cycle → agent `send_email` reply → recorded outbound.
- Maintain the repo's ≥80% coverage expectation (workload `CLAUDE.md`).

## 6. Out of scope (v1)

Attachments, HTML bodies, threading/references, multiple mailboxes per company, manager-side push/forwarding (poll only), and any change to the manager (Phase B.1 is done). These are candidate follow-ons.

## 7. Open questions / residual

- **UID vs `\Seen` dedup:** v1 uses `\Seen`. If a human ever shares the mailbox, switch to tracking the last-seen IMAP UID persisted per company. Low-risk to change later (isolated to `AsyncImapReceiver`).
- **IMAP crate choice:** `async-imap` is the default; confirm it builds cleanly against the existing tokio/rustls stack at implementation (fallback: `imap` sync in a blocking task).
- **Poller ↔ scale-to-zero coordination:** the poller stops on sleep; confirm the manager's idle detection isn't kept awake by IMAP connections (poll should be a short-lived connection per tick, not a persistent IDLE).
