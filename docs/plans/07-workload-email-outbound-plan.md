# WS7 Outbound — `send_email`: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** An approval-gated `send_email` agent tool that sends from the company's own mailbox via `MailSender`, gated as an `email.send` `Effect` (Send group), with replies to established (prior-inbound) recipients auto-allowed and cold recipients parked for approval.

**Architecture:** Intercept `send_email` at `CycleHostImpl::call_tool` → build an `email.send` `Effect` → run the same evaluate/execute/park path `emit_effect` uses → executor branch in `perform_effect` calls `MailSender`. The sender + creds are injected onto `CompanyRuntime` (new `mail` field). No `vendor/` changes.

**Tech Stack:** Rust 2024, `#[async_trait]`, `serde_json`, existing `MailSender`/`SmtpCredentials`/`InboxStore`/`ApprovalGate`.

**Design source:** [`07-workload-email-outbound-spec.md`](07-workload-email-outbound-spec.md).

## Global Constraints

- Before **every** commit: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build --all-targets`, `cargo test` all pass. Needs `git submodule update --init --recursive vendor` (already done). Default toolchain, plain `cargo`. `smtp`-feature paths also verify under `--features smtp` (build + clippy).
- All traits use `#[async_trait]`. Errors use `OpenCompanyError` (`InvalidRequest` for bad args / no mail; `Config` for provider mismatch; sender returns its own `OpenCompanyError`).
- **Verified seams (use exactly):** `Effect{kind,group,amount_usd,established_thread,first_time_counterparty,payload}` (`ports/types.rs:411`); `EffectGroup::Send`; gate `Allow` iff `established_thread && !first_time_counterparty` (`policy/gate.rs:179-186`); `CycleHostImpl{company,cycle_id,rt,counter,executed,parked}` + `emit_effect` (`runtime/cycle.rs:283-369`); `execute_effect_once`/`perform_effect` (`cycle.rs:227-279`); `ToolCall{tool,args}` / `ToolResult{ok,output}` (`ports/types.rs:603`); `InboxStore::messages(company,key,limit,offset)` + `EmailRecord{...,from_email,outbound}` (`ports/inbox.rs`); `record_outbound` + `local_part` + `SmtpCredentials` (`server/ops/smtp.rs`); `MailSender::send(&MailCredentials, &OutboundEmail)` + `MailCredentials::Smtp` + `OutboundEmail{to,subject,body}` (`server/ops/mailer.rs`).
- **From identity:** every send uses the tenant's own address (`rt.mail.smtp.from_email`); ignore any `from` in args.

## File Structure

- Modify: `src/company/runtime.rs` — `CompanyMail` type, `mail: Option<CompanyMail>` field, `mail()` accessor, `new(...)` param.
- Modify: `src/runtime/builder.rs` — `mail` builder field + `with_mail` + build wiring.
- Modify: `src/runtime/cycle.rs` — `send_email` interception in `call_tool`, shared gate helper, `established` lookup, `email.send` branch in `perform_effect`.
- Modify: `src/feedback/tool.rs` (or wherever the builtin catalog is assembled) — advertise the `send_email` `ToolSpec`.
- Modify: `src/bin/opencompany.rs` — seed `with_mail` from `TenantMailboxConfig` under `#[cfg(feature="smtp")]`.

---

### Task 1: `CompanyMail` + `CompanyRuntime.mail` field + builder

**Files:** Modify `src/company/runtime.rs`, `src/runtime/builder.rs`

**Interfaces:**
- Produces: `pub struct CompanyMail { pub sender: Arc<dyn MailSender>, pub smtp: SmtpCredentials }`; `CompanyRuntime.mail: Option<CompanyMail>` + `pub fn mail(&self) -> Option<&CompanyMail>`; `RuntimeBuilder::with_mail(self, CompanyMail) -> Self`.

- [ ] **Step 1: Add `CompanyMail` + field + accessor** in `src/company/runtime.rs`:

```rust
use crate::server::ops::mailer::MailSender;
use crate::server::ops::smtp::SmtpCredentials;

/// The company's own outbound-mail handle: a sender + its SMTP credentials
/// (the manager-injected per-tenant mailbox). `None` when email isn't wired.
#[derive(Clone)]
pub struct CompanyMail {
    pub sender: Arc<dyn MailSender>,
    pub smtp: SmtpCredentials,
}
```
Add field to the struct (near `inbox`): `pub(crate) mail: Option<CompanyMail>,` and accessor near `inbox()`:
```rust
pub fn mail(&self) -> Option<&CompanyMail> { self.mail.as_ref() }
```
Add a `mail: Option<CompanyMail>` parameter to `CompanyRuntime::new(...)` (it already has `#[allow(clippy::too_many_arguments)]`) and assign it.

- [ ] **Step 2: Builder wiring** in `src/runtime/builder.rs` — add field `mail: Option<CompanyMail>,` (default `None`), setter, and pass through at `build()`:

```rust
/// Wires the company's outbound mail sender + credentials. Absent by default
/// (email send is opt-in / hosted-only).
pub fn with_mail(mut self, mail: crate::company::runtime::CompanyMail) -> Self {
    self.mail = Some(mail);
    self
}
```
In `build()`, pass `self.mail` into `CompanyRuntime::new(...)` at the new parameter position (no default fill — it's `Option`).

- [ ] **Step 3: Build + existing tests pass** (call-site of `CompanyRuntime::new` in the builder is the only one; the compiler will flag any others — pass `None`).

Run: `cargo build --all-targets && cargo test company:: runtime::`
Expected: compiles; existing tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/company/runtime.rs src/runtime/builder.rs
git commit -m "feat(email): CompanyMail handle on CompanyRuntime (+ builder)"
```

---

### Task 2: `perform_effect` executor for `email.send`

**Files:** Modify `src/runtime/cycle.rs`

**Interfaces:** Consumes `CompanyRuntime.mail`. Produces: an `email.send` branch in `perform_effect` that sends + records; helper `send_company_email(rt, to, subject, body)`.

- [ ] **Step 1: Write the failing test** — an `email.send` effect with a mock sender sends + records; missing `rt.mail` errors. Model the test `CompanyRuntime` on the existing `cycle.rs` test module; inject `mail: Some(CompanyMail{ sender: Arc::new(RecordingMailSender::new()), smtp: <test creds> })`.

```rust
#[tokio::test]
async fn email_send_effect_sends_and_records() {
    let sender = Arc::new(RecordingMailSender::new());
    let rt = /* test CompanyRuntime with mail = Some(CompanyMail{ sender: sender.clone(), smtp: test_smtp("ceo@acme.test") }) */;
    let effect = Effect {
        kind: "email.send".into(), group: EffectGroup::Send, amount_usd: None,
        established_thread: true, first_time_counterparty: false,
        payload: serde_json::json!({ "to": "x@ext.com", "subject": "Hi", "body": "yo" }),
    };
    perform_effect(&rt, &effect).await.unwrap();
    assert_eq!(sender.sent().len(), 1);                       // mock exposes sent()
    let inbox = rt.inbox().messages(rt.id(), "ceo", 10, 0).await.unwrap();
    assert!(inbox.iter().any(|r| r.outbound && r.subject == "Hi"));
}
```
(If `RecordingMailSender` has no `sent()` accessor, add one in this task — a `Mutex<Vec<OutboundEmail>>` recorder, mirroring `RecordingMailReceiver`.)

Run: `cargo test email_send_effect_sends_and_records` → FAIL.

- [ ] **Step 2: Add the executor** — in `perform_effect` (`cycle.rs`), after the existing channel block:

```rust
    if effect.kind == "email.send" {
        let to = effect.payload.get("to").and_then(|v| v.as_str()).unwrap_or_default();
        let subject = effect.payload.get("subject").and_then(|v| v.as_str()).unwrap_or_default();
        let body = effect.payload.get("body").and_then(|v| v.as_str()).unwrap_or_default();
        send_company_email(rt, to, subject, body).await?;
    }
```
And the helper (module-level in `cycle.rs`), reusing `record_outbound`'s shape:

```rust
use crate::server::ops::mailer::{MailCredentials, OutboundEmail};

async fn send_company_email(rt: &CompanyRuntime, to: &str, subject: &str, body: &str) -> Result<()> {
    let Some(mail) = rt.mail() else {
        return Err(OpenCompanyError::InvalidRequest("email is not configured for this company".into()));
    };
    let email = OutboundEmail { to: to.to_string(), subject: subject.to_string(), body: body.to_string() };
    mail.sender.send(&MailCredentials::Smtp(mail.smtp.clone()), &email).await?;
    // Record to the sender's own inbox (from = the company's own address).
    crate::server::ops::smtp::record_outbound(rt, &mail.smtp, &email).await;
    Ok(())
}
```
Ensure `record_outbound` is `pub(crate)` (it is per the research; if not, make it so).

- [ ] **Step 3: Run + gate**

Run: `cargo test cycle:: && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/runtime/cycle.rs src/server/ops/mailer.rs
git commit -m "feat(email): perform_effect executes email.send via MailSender"
```

---

### Task 3: `established` lookup helper

**Files:** Modify `src/runtime/cycle.rs`

**Interfaces:** Produces `async fn recipient_is_established(rt: &CompanyRuntime, to: &str) -> bool` — true iff a prior **inbound** `EmailRecord` from `to` exists.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn established_true_only_after_inbound_from_recipient() {
    let rt = /* test CompanyRuntime, address ceo@acme.test */;
    assert!(!recipient_is_established(&rt, "x@ext.com").await);
    // seed an inbound record from x@ext.com
    rt.inbox().append(rt.id(), &EmailRecord {
        id: "1".into(), inbox: "ceo".into(), from_name: "".into(),
        from_email: "x@ext.com".into(), subject: "hi".into(), body: "".into(),
        at_millis: 0, read: false, outbound: false,
    }).await.unwrap();
    assert!(recipient_is_established(&rt, "X@EXT.COM").await); // case-insensitive
}
```

Run: `cargo test established_true_only_after_inbound_from_recipient` → FAIL.

- [ ] **Step 2: Implement** (bounded scan of the company's own inbox):

```rust
async fn recipient_is_established(rt: &CompanyRuntime, to: &str) -> bool {
    let key = crate::server::ops::smtp::local_part(&company_address(rt));
    let to = to.trim().to_ascii_lowercase();
    match rt.inbox().messages(rt.id(), &key, 500, 0).await {
        Ok(records) => records.iter().any(|r| !r.outbound && r.from_email.trim().to_ascii_lowercase() == to),
        Err(_) => false, // fail closed → parks for approval
    }
}
```
`company_address(rt)` = the tenant address. Source it from `rt.mail().map(|m| m.smtp.from_email.clone())`; if `None`, no mail configured so establishment is moot. Implement `company_address` accordingly (return `rt.mail()`'s from_email or empty).

- [ ] **Step 3: Run + gate**

Run: `cargo test cycle:: && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/runtime/cycle.rs
git commit -m "feat(email): established-thread lookup (prior inbound from recipient)"
```

---

### Task 4: Intercept `send_email` in `call_tool` → gated effect

**Files:** Modify `src/runtime/cycle.rs`

**Interfaces:** Consumes Tasks 2-3. Produces the `send_email` branch in `CycleHostImpl::call_tool` + a shared `gate_effect` method factored from `emit_effect`.

- [ ] **Step 1: Factor the gate logic** — extract the evaluate/execute/park body of `emit_effect` into `async fn gate_effect(&self, effect: Effect) -> Result<EffectDisposition>` and have `emit_effect` call it. (Pure refactor; existing effect tests must still pass.)

- [ ] **Step 2: Add the `call_tool` interception** — at the top of `CycleHostImpl::call_tool`:

```rust
    async fn call_tool(&self, call: ToolCall) -> Result<ToolResult> {
        if call.tool == "send_email" {
            return self.send_email(call.args).await;
        }
        self.rt.tools.invoke(&self.company, call).await
    }
```
And the method on `CycleHostImpl`:

```rust
    async fn send_email(&self, args: serde_json::Value) -> Result<ToolResult> {
        let get = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let (Some(to), Some(subject), Some(body)) = (get("to"), get("subject"), get("body")) else {
            return Ok(ToolResult { ok: false, output: serde_json::json!({ "error": "send_email requires to, subject, body" }) });
        };
        if to.trim().is_empty() {
            return Ok(ToolResult { ok: false, output: serde_json::json!({ "error": "recipient (to) is empty" }) });
        }
        let established = recipient_is_established(self.rt, &to).await;
        let effect = Effect {
            kind: "email.send".into(), group: EffectGroup::Send, amount_usd: None,
            established_thread: established, first_time_counterparty: !established,
            payload: serde_json::json!({ "to": to, "subject": subject, "body": body }),
        };
        match self.gate_effect(effect).await? {
            EffectDisposition::Executed => Ok(ToolResult { ok: true, output: serde_json::json!({ "status": "sent" }) }),
            EffectDisposition::PendingApproval(id) => Ok(ToolResult { ok: true, output: serde_json::json!({ "status": "pending_approval", "approval_id": id.as_ref() }) }),
            EffectDisposition::Denied { reason } => Ok(ToolResult { ok: false, output: serde_json::json!({ "status": "denied", "reason": reason }) }),
        }
    }
```

- [ ] **Step 3: Write the tests** — drive `CycleHostImpl` directly (construct it like the existing cycle tests do, or via a small helper). Two cases:

```rust
#[tokio::test]
async fn send_email_parks_for_new_recipient() {
    let sender = Arc::new(RecordingMailSender::new());
    let host = /* CycleHostImpl over a test rt with mail=Some(sender), default manifest approval policy */;
    let res = host.send_email(serde_json::json!({"to":"new@ext.com","subject":"s","body":"b"})).await.unwrap();
    assert_eq!(res.output["status"], "pending_approval");
    assert_eq!(sender.sent().len(), 0);
}

#[tokio::test]
async fn send_email_sends_for_established_recipient() {
    let sender = Arc::new(RecordingMailSender::new());
    let rt = /* test rt, mail=Some(sender) */;
    rt.inbox().append(rt.id(), &inbound_from("known@ext.com")).await.unwrap();
    let host = /* CycleHostImpl over rt */;
    let res = host.send_email(serde_json::json!({"to":"known@ext.com","subject":"s","body":"b"})).await.unwrap();
    assert_eq!(res.output["status"], "sent");
    assert_eq!(sender.sent().len(), 1);
}
```
(Reuse the manifest approval gate the cycle tests already build; the default policy parks first-time Send. If the default test manifest auto-allows everything, seed a manifest whose Send policy is the default supervised one — mirror `policy/gate.rs` tests.)

Run: `cargo test cycle::` → the new tests + existing pass.

- [ ] **Step 4: Gate + commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`
```bash
git add src/runtime/cycle.rs
git commit -m "feat(email): send_email tool -> approval-gated email.send effect"
```

---

### Task 5: Advertise `send_email` in the builtin tool catalog

**Files:** Modify `src/feedback/tool.rs` (the `BuiltinToolProvider` catalog)

**Interfaces:** `send_email` appears in `catalog()` with schema `{to,subject,body}` (all required). Execution is NOT added here (it's intercepted in `call_tool`); the provider only advertises it.

- [ ] **Step 1: Write the failing test** — `catalog()` includes a `send_email` spec with the three required fields.

```rust
#[tokio::test]
async fn catalog_advertises_send_email() {
    let provider = /* BuiltinToolProvider as built in tests */;
    let specs = provider.catalog(&CompanyId::new("acme")).await.unwrap();
    let spec = specs.iter().find(|s| s.name == "send_email").expect("send_email advertised");
    let req = &spec.input_schema["required"];
    assert!(req.as_array().unwrap().iter().any(|v| v == "to"));
}
```

- [ ] **Step 2: Add the spec** — a `send_email_spec()` returning `ToolSpec{ name:"send_email", description:"Send an email from your company mailbox to a recipient. First emails to a new recipient require operator approval.", input_schema: json!({type:object, properties:{to,subject,body:{type:string}}, required:["to","subject","body"]}) }`, and push it in `catalog()` alongside the feedback spec.

- [ ] **Step 3: Run + gate + commit**

Run: `cargo test feedback:: && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`
```bash
git add src/feedback/tool.rs
git commit -m "feat(email): advertise send_email in the builtin tool catalog"
```

---

### Task 6: Seed `with_mail` in `serve`

**Files:** Modify `src/bin/opencompany.rs`

**Interfaces:** When `TenantMailboxConfig::from_env()` is `Some` and (`#[cfg(feature="smtp")]`) a `LettreMailSender` exists, the per-company `RuntimeBuilder` gets `with_mail(CompanyMail{...})`.

- [ ] **Step 1: Wire it** where the company `RuntimeBuilder` is assembled in `register_company` (near `with_inbox`/`with_stores`):

```rust
    #[cfg(feature = "smtp")]
    if let Ok(Some(cfg)) = opencompany::server::ops::mailer::TenantMailboxConfig::from_env() {
        builder = builder.with_mail(opencompany::company::runtime::CompanyMail {
            sender: std::sync::Arc::new(opencompany::server::ops::smtp::LettreMailSender),
            smtp: cfg.smtp.clone(),
        });
    }
```
(If `register_company` is not `#[cfg(feature="smtp")]`-aware, guard just this block. Under `--no-default`/no-smtp, send_email will return "email not configured" — acceptable.)

- [ ] **Step 2: Build both feature states + full suite**

Run: `cargo build --all-targets && cargo build --all-targets --features smtp && cargo test && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features smtp -- -D warnings && cargo fmt --all -- --check`
Expected: all PASS.

- [ ] **Step 3: Commit**

```bash
git add src/bin/opencompany.rs
git commit -m "feat(email): wire company mail sender in serve (smtp feature)"
```

---

## Post-implementation (not code tasks)

- **Live round-trip smoke test** (with `--features smtp,imap`, real tenant): agent `send_email` to a fresh address → parks; approve → delivers, signed, from `<slug>@`; a reply arrives via the poller. Then `send_email` to that same (now-established) address → sends without approval.
- **Follow-up:** store the recipient on outbound `EmailRecord`s so "we emailed them first" also counts as established (bidirectional), and consider per-tenant send quotas.
- **Docs:** note `send_email` + the approval semantics in `docs/modules/` / the tool catalog docs.
