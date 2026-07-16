# 06 — WS6: Connections (OAuth), Domain/DNS, SMTP & Email

## Scope

The credential-bearing surfaces: OAuth connect/disconnect for the connections
catalog, custom-domain provisioning with DNS verification, SMTP credentials +
test send, and the email transport that makes per-teammate inboxes real.
Everything secret-shaped lives in `SecretStore`; the default build stays
offline (all network I/O feature-gated).

## Design

### Connections OAuth

Routes (dual-scoped like all WS3 ops routes, except the callback):

| Method | Path | Behavior |
|---|---|---|
| POST | `…/connections/{provider}/start` | → `{url}`: provider authorize URL with a signed `state` nonce (company+provider+expiry), redirect URI from config |
| GET | `/api/v1/oauth/callback` (unscoped) | verifies `state`, exchanges `code` server-side, stores tokens at `SecretStore["oauth/{provider}"]`, captures the account label, redirects back to the console |
| POST | `…/connections/{provider}/disconnect` | best-effort revoke, delete the secret |

- **Callback hosting:** this crate hosts it. In managed deployments the
  manager may front the same path; `OPENCOMPANY_OAUTH_REDIRECT_BASE`
  overrides the redirect URI so the authorize URL points wherever the
  operator's browser can reach (README open question resolution).
- **Provider app credentials** (client_id/client_secret) are host-level
  configuration (`OPENCOMPANY_OAUTH_<PROVIDER>_ID/_SECRET`), never
  per-company; per-company state is tokens only.
- Read side (GraphQL `connections`): catalog priorities from `[[connection]]`
  manifest intent ∪ connected state from the secret store — `{provider,
  connected, account?, reason?}`. **Account label and connected flag only;
  token material never appears in any response** (feature-tested).
- Token exchange needs `reqwest` → gate the write routes behind a small
  `oauth` feature; without it they 404 and the console shows the read-only
  catalog (existing seam behavior).

### Domain & DNS

| Method | Path | Behavior |
|---|---|---|
| PUT | `…/domain` | `{domain}` → `DomainStatus` with generated records |
| POST | `…/domain/verify` | server-side DNS lookups → updated `DomainStatus` |

- `src/company/dns.rs`: pure record generation — verification TXT
  (deterministic token derived per company+domain), mail CNAME, two DKIM
  CNAMEs, SPF TXT — mirroring `frontend/src/lib/domain.ts::dnsRecords` so the
  console renders identically.
- Verification via a mockable trait:

```rust
pub trait DnsResolver: Send + Sync {
    async fn txt(&self, name: &str) -> Result<Vec<String>>;
    async fn cname(&self, name: &str) -> Result<Option<String>>;
}
```

Real impl uses `hickory-resolver` behind feature `dns`; without the feature
`verify` returns the not-wired 404. Config JSON persists at
`SecretStore["__domain"]`.

### SMTP & outbound email

| Method | Path | Behavior |
|---|---|---|
| PUT | `…/smtp` | credentials → `SecretStore["__smtp"]`; response is non-secret `SmtpStatus` |
| POST | `…/smtp/test` | `{to?}` → `{ok, message}` — sends a test email |

Sending via a mockable `MailSender` trait; the real impl uses `lettre` behind
feature `smtp`, pulling credentials from the secret store per send. Every
outbound send also appends an `EmailRecord { outbound: true }` to `InboxStore`
so the console shows sent mail. The WS4 harness exposes a `send_email` tool to
agents whose inbox is enabled — gated by the approval policy
(`external.publish`-class effect).

### Inbound email

Reuse the webhook surface already specified in
[`docs/spec/runtime/api.md`](../spec/runtime/api.md) rather than inventing a
new route:

```
POST /hooks/{companyId}/email
```

HMAC-verified per-channel secret from `SecretStore` (the existing `webhooks`
feature signer); unverifiable payloads are dropped with 401 and never become
events. The handler parses the payload (from/subject/body/to), resolves the
target inbox by address, appends to `InboxStore`, and enqueues a
`CompanyEvent::WebhookReceived`-derived event so the addressed teammate can
act on the mail. A mail-forwarding provider or the platform manager (which
owns MX in managed deployments) pushes into this hook — no IMAP polling in v1
(README open question 2).

Inbox addresses: `{agent_id}@{domain}` once a domain is verified; before
that, inboxes exist but are marked "pending domain" in the read surface.

## Subtasks (commit-sized)

1. `feat(company): dns record generation + DnsResolver trait` (pure, no features)
2. `feat(server): domain routes + verify behind feature dns`
3. `feat(server): smtp routes + MailSender behind feature smtp`
4. `feat(server): oauth start/callback/disconnect behind feature oauth`
5. `feat(server): inbound email hook -> InboxStore + event`
6. `feat(harness): send_email tool wired to MailSender + approval policy`

1–5 are parallelizable across subagents (distinct files); 6 lands after WS4.

## Dependencies

WS3 (`InboxStore`, ops scaffolding, SecretStore keys). Feeds WS7
(Connections, Settings, Inbox views) and completes WS3's inbox surface.

## Tests & exit criteria

Unit: DNS record generation determinism; mock-resolver verify outcomes.
Feature: the four-case suite per route plus — no token/password material in
any serialized response; callback with tampered `state` → 401; inbound hook
with bad HMAC → 401 and no event; smtp test with mock sender. Default build
compiles with no network crates. Exit per
[09-verification.md](09-verification.md) WS6 row.
