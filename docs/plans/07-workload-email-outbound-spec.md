# WS7 Outbound — `send_email`: Design Spec

**Status:** Design (approved for planning) · **Date:** 2026-07-28
**Builds on:** [`07-workload-email-send-receive.md`](07-workload-email-send-receive.md) (spec) + the inbound plan (shipped, PR #143). This is the **outbound** half.

---

## 1. Goal

Give the agent a `send_email` tool so a company can send mail **from its own** `<slug>@opencompany.work` address — **approval-gated** so it can't email the world unchecked and burn the shared domain's reputation.

**Success =** the agent calls `send_email{to,subject,body}`; a first message to a new recipient **parks for operator approval**; a reply to an address we've already corresponded with **sends immediately**, signed, from the company's own mailbox, and is recorded as an outbound `EmailRecord`.

## 2. The architectural constraint (from the spike)

The live brain is a hosted, closed service we can't modify, and **tools execute immediately** (`CycleHost::call_tool` → `tools.invoke`, ungated) — only **`Effect`s** emitted through the approval gate are policy-checked. So outbound email must become a gated `Effect`, but we can't make the brain emit one. Resolution: **intercept the `send_email` tool call and turn it into an `email.send` Effect ourselves**, at the single chokepoint every brain hits. No `vendor/` changes.

## 3. Locked decisions

| # | Decision | Choice |
|---|---|---|
| 1 | Trigger | A `send_email` **tool** the agent calls (advertised in the builtin catalog). |
| 2 | Gating | Intercept at `CycleHostImpl::call_tool` → build an `Effect{kind:"email.send", group:Send}` → run the **same** gate as `emit_effect` (evaluate → execute / park / deny). |
| 3 | Approval rule | **Established = the recipient has emailed us before** (a prior *inbound* `EmailRecord` with `from_email == to`). Then `established_thread=true` + `first_time_counterparty=false` → policy **auto-allows** (reply freely). Otherwise (cold recipient) → `first_time_counterparty=true` → **parks** for approval. *Refinement from the original "we've emailed them" wording:* outbound records don't store the recipient today, so "we emailed them" isn't detectable without a schema change; "they emailed us" is, and it's the reputation-safe direction. Extending to outbound-recipient detection = a documented follow-up (add a `to` field to outbound `EmailRecord`s). The gate `Allow`s **only** when `established_thread && !first_time_counterparty` (`policy/gate.rs:179-186`), so both flags must be set accordingly. |
| 4 | Executor | New `email.send` branch in `perform_effect` → `MailSender::send` with the tenant's SMTP creds → `record_outbound`. |
| 5 | Sender wiring | Inject an optional `mail` handle into `CompanyRuntime` via `RuntimeBuilder` (mirroring `inbox`/`secrets`), seeded from `TenantMailboxConfig.smtp`. When absent, `send_email` returns a clear "email not configured" `ToolResult`. |
| 6 | From identity | Always the company's own `OPENCOMPANY_MAIL_ADDRESS`; the agent cannot spoof another sender. Plain-text body (v1). |

## 4. Design

### 4.1 Advertise the tool
Add a `send_email` `ToolSpec` (name, description, `input_schema` = `{to, subject, body}` all required) to the builtin catalog (the `BuiltinToolProvider`/`feedback` pattern). This only makes the agent *aware* of the tool; execution is intercepted before the provider (§4.2).

### 4.2 Intercept → gated effect
In `CycleHostImpl::call_tool` (`src/runtime/cycle.rs`), before delegating to `self.rt.tools.invoke(...)`:
- if `call.tool == "send_email"`: parse `{to, subject, body}` from `call.args` (bad/missing args → `ToolResult{ok:false, ...}`, no effect);
- compute `established` = does `InboxStore` hold any prior message to/from `to` for this company (§4.3);
- build `Effect{ kind:"email.send", group:EffectGroup::Send, amount_usd:None, established_thread: established, first_time_counterparty: !established, payload: {to, subject, body} }`;
- run it through the **same** logic `emit_effect` uses (`self.rt.approvals.evaluate` → `Allow`: execute via the effect executor + record; `RequireApproval`: park + return the approval id; `Deny`: reject);
- return a `ToolResult`: `{ok:true, output:{status:"sent"}}` or `{ok:true, output:{status:"pending_approval", approval_id}}` or `{ok:false, output:{error}}`.

Factor the shared gate logic so `call_tool`'s email path and `emit_effect` don't duplicate the evaluate/park/execute sequence.

### 4.3 Established-thread lookup
`established(to)` = scan `InboxStore::messages(company, inbox_key, limit, 0)` for any **inbound** record (`!outbound`) whose `from_email` equals `to` (case-insensitive). `inbox_key` = the company's own local-part (`local_part(address)`). Bound the scan (recent N, e.g. 500) for cost. Outbound records can't contribute today (no recipient stored) — see Decision 3.

### 4.4 Execute
Add to `perform_effect` (`src/runtime/cycle.rs`): when `effect.kind == "email.send"`, read `to/subject/body` from `payload`, and if `rt.mail` is present, `mail.sender.send(&mail.creds_as_MailCredentials, &OutboundEmail{to,subject,body})` then `record_outbound(rt, &smtp_creds, &email)`. A missing `rt.mail` at execute time → `OpenCompanyError::InvalidRequest("email not configured")` (parked effects executed after approval hit the same check).

### 4.5 Sender wiring
- New `CompanyRuntime` field `mail: Option<CompanyMail>` where `CompanyMail { sender: Arc<dyn MailSender>, smtp: SmtpCredentials }` (both the sender **and** the tenant's creds live here — the injected `OPENCOMPANY_MAIL_*` creds are NOT in `SecretStore["__smtp"]`, so `perform_effect` reads them from `rt.mail`, not `load_credentials`).
- `RuntimeBuilder`: `mail: Option<CompanyMail>` field + `with_mail(...)` setter + `unwrap`-to-`None` default at `build()` (mirror the `economy: Option<...>` optional pattern, not the always-defaulted `inbox`). Thread it into `CompanyRuntime::new(...)`.
- In `src/bin/opencompany.rs`: when `TenantMailboxConfig::from_env()` is `Some` and (under `#[cfg(feature="smtp")]`) a `LettreMailSender` is available, `builder = builder.with_mail(CompanyMail{ sender: Arc::new(LettreMailSender), smtp: cfg.smtp.clone() })` at company registration (same spot the poller is wired).

## 5. Test plan

- **T1 gate — new recipient parks:** empty inbox → `send_email` to a new address → effect parked, `ToolResult` status `pending_approval`, `MailSender` NOT called (mock).
- **T2 gate — established sends:** seed an inbox record for `to` → `send_email` → `MailSender::send` called once, outbound `EmailRecord` recorded, status `sent`.
- **T3 executor:** an `email.send` effect through `perform_effect` with a mock sender → sends + records; missing `rt.mail` → `InvalidRequest`.
- **T4 args:** missing/blank `to` → `ToolResult{ok:false}`, no effect.
- **T5 catalog:** `send_email` appears in the builtin catalog with the right schema.
- **T6 identity:** the sent `OutboundEmail`/record `from` is the company's own address regardless of `args`.
- Mocks only (`RecordingMailSender`); no live SMTP. Maintain ≥80% coverage.

## 6. Out of scope (v1)

Attachments, HTML, threading headers, CC/BCC, per-recipient rate limits beyond Stalwart's, and any UI for resolving parked approvals (that flow already exists in the approvals surface). Manager unchanged.

## 7. Open questions / residual

- **Exact `ApprovalGate`/`emit_effect` internals** (evaluate/park signatures, the cycle-id/counter used for the journal key) — confirm against `src/runtime/cycle.rs` + `src/ports/approvals.rs` at plan time so the shared gate helper matches.
- **Counterparty match** — start with exact address equality; normalize case; revisit display-name/alias matching later.
